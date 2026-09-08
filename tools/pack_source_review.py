#!/usr/bin/env python3
"""Create private reviews or public source candidates; never publish or deploy."""
import argparse
import gzip
import hashlib
import io
import ipaddress
import json
import posixpath
from pathlib import Path, PurePosixPath
import re
import tarfile
from urllib.parse import unquote

# Deliberately exact: additions require review; no recursive copying or Git history.
ALLOWLIST = ('.github/workflows/ci.yml', '.gitignore', 'CONTRIBUTING.md', 'Cargo.lock', 'Cargo.toml', 'LICENSE', 'NOTICE', 'README.md', 'SECURITY.md', 'docs/architecture.md', 'docs/comparison.md', 'docs/coordinator-work.md', 'docs/execution-work.md', 'docs/guarantees.md', 'docs/kernel-assisted-delta.md', 'docs/local-supervision-verification.md', 'docs/milestone-1-verification.md', 'docs/release.md', 'docs/sdk-adapter-work.md', 'docs/soak-harness.md', 'docs/validation.md', 'examples/cpu-job.json', 'examples/gpu-pressure.json', 'examples/node.yaml', 'python/LICENSE', 'python/NOTICE', 'python/README.md', 'python/examples/counter.py', 'python/pyproject.toml', 'python/resmgr/__init__.py', 'python/resmgr/client.py', 'python/resmgr/process.py', 'python/resmgr/worker.py', 'python/tests/test_process.py', 'python/tests/test_worker.py', 'src/agent.rs', 'src/artifacts.rs', 'src/backup.rs', 'src/cgroup.rs', 'src/child_supervision.rs', 'src/config.rs', 'src/coordinator.rs', 'src/execution_model.rs', 'src/execution_state.rs', 'src/kernel.rs', 'src/lib.rs', 'src/main.rs', 'src/managed_children.rs', 'src/model.rs', 'src/policy.rs', 'src/protocol.rs', 'src/rootless.rs', 'src/state.rs', 'src/storage_qualification.rs', 'src/supervision.rs', 'src/telemetry.rs', 'tests/agent.rs', 'tests/agent_reconciliation.rs', 'tests/artifacts.rs', 'tests/backup.rs', 'tests/cgroup.rs', 'tests/cli.rs', 'tests/coordinator.rs', 'tests/distributed_service.rs', 'tests/execution_state.rs', 'tests/family_contract.rs', 'tests/gpu_native.rs', 'tests/history_overhead.rs', 'tests/kernel.rs', 'tests/managed_children.rs', 'tests/managed_supervision.rs', 'tests/policy.rs', 'tests/replay_state.rs', 'tests/state.rs', 'tests/storage_qualification.rs', 'tests/supervision.rs', 'tests/telemetry.rs', 'tests/test_anchor_validation.py', 'tests/test_bundle.py', 'tests/test_compare.py', 'tests/test_comparison_resume.py', 'tests/test_connection_proxy.py', 'tests/test_connection_proxy_stream.py', 'tests/test_container_validation_profile.py', 'tests/test_cpu_command_comparison.py', 'tests/test_local_smoke_stress.py', 'tests/test_operations.py', 'tests/test_pressure_comparison.py', 'tests/test_sdk_client_transport.py', 'tests/test_session_transfer.py', 'tests/test_source_review.py', 'tests/test_two_node_bootstrap.py', 'tests/test_two_node_validation.py', 'tests/test_validation_tools.py', 'tests/tls_transport.rs', 'tools/anchor_validation.py', 'tools/compare.py', 'tools/connection_proxy.py', 'tools/local_smoke.py', 'tools/make_kaggriculture_validation.py', 'tools/make_soak_config.py', 'tools/make_test_pki.py', 'tools/native_validation.py', 'tools/pack_source_review.py', 'tools/pack_validation.py', 'tools/pressure.py', 'tools/pressure_comparison.py', 'tools/pressure_probe.py', 'tools/pressure_schedule.py', 'tools/review_local_smoke.py', 'tools/session_transfer.py', 'tools/soak.py', 'tools/storage_qualification.py', 'tools/two_node_bootstrap.py', 'tools/two_node_validation.py', 'tools/unpack_validation.py', 'tools/validation_runtime.py')
# These are part of the reviewed source closure, not bootstrap/toolchain archives.
ALLOWLIST += ('.github/workflows/sdk-preparation.yml',
              'docs/release-0.2.0-preparation.md',
              'python/tests/test_client_release_repairs.py',
              'python/tests/test_release_repairs.py')

NOTICE = """This is a private source review, not a published release. Included license files
retain their terms; this exporter does not publish or grant additional rights.
Raw deployment evidence, credentials, local work logs, application datasets and
model assets are deliberately excluded. Validation summaries retain their stated
scope and limitations; this archive is not independently sufficient to reproduce
historical measurements. Source and SDK code are unchanged by export. See
[validation](docs/validation.md) and [guarantees](docs/guarantees.md).
"""
LINK = re.compile(r'\[([^\]]+)\]\(([^)]+)\)')
IPV4 = re.compile(r'(?<![\w.])(?:\d{1,3}\.){3}\d{1,3}(?![\w.])')
DOCUMENTATION_NETWORKS = tuple(ipaddress.ip_network(n) for n in
                               ('192.0.2.0/24', '198.51.100.0/24', '203.0.113.0/24'))


def checked_home(path):
    path = Path(path).absolute()
    path.resolve().relative_to(Path.home().resolve())
    return path


def safe_relative(name):
    path = PurePosixPath(name)
    if path.is_absolute() or '..' in path.parts or str(path) != name:
        raise ValueError('unsafe export path')
    return path


def read_regular(root, name):
    parts = safe_relative(name).parts
    path = root
    for part in parts:
        path = path / part
        if path.is_symlink():
            raise ValueError('symlinks are forbidden in source review')
    if not path.is_file():
        raise ValueError('required export file is missing: ' + name)
    return path.read_bytes()


def curate(name, data, public=False):
    if not name.endswith('.md'):
        return data
    value = data.decode('utf-8')
    def rewrite(match):
        label, target = match.groups()
        target_path = target.split('#', 1)[0]
        if 'artifacts/' in target_path:
            return label + ' (private evidence retained outside this source review)'
        if target_path.endswith('work-log.md'):
            return '[validation summary](' + target.replace('work-log.md', 'validation.md') + ')'
        return match.group(0)
    value = LINK.sub(rewrite, value)
    value = value.replace('durable [validation summary]', '[validation summary]')
    if name == 'README.md':
        first, rest = value.split('\n', 1)
        value = first + '\n\n' + ("CedeGrid source distribution. See [LICENSE](LICENSE), [release scope](docs/release.md) and [guarantees](docs/guarantees.md).\n" if public else NOTICE) + '\n' + rest
    elif name in ('docs/validation.md', 'docs/comparison.md'):
        first, rest = value.split('\n', 1)
        value = first + '\n\nSource distribution: linked private deployment evidence is withheld.\n' \
            'All reported failures, unsupported capabilities and pending gates remain applicable.\n\n' + rest
    return value.encode('utf-8')


def inspect_text(name, data):
    text = data.decode('utf-8')
    # These checks find common accidental disclosures, not every possible secret.
    if re.search(r'-----BEGIN (?:[A-Z]+ )?PRIVATE KEY-----', text):
        raise ValueError('private key material in ' + name)
    if re.search(r'https://login[.]tailscale[.]com/a/[a-zA-Z0-9]+', text):
        raise ValueError('enrollment URL in ' + name)
    for user in re.findall(r'/(?:Users|home)/(?:students/cs/)?([^/\s"\x27`]+)', text):
        if user not in ('USER', 'user', 'test', 'resmgr', 'example', '<USER>', '<user>') and not user.startswith(('$', '{')):
            raise ValueError('non-placeholder home identity in ' + name)
    for address in IPV4.findall(text):
        try:
            ip = ipaddress.ip_address(address)
        except ValueError:
            continue
        if not (ip.is_loopback or ip.is_unspecified or any(ip in n for n in DOCUMENTATION_NETWORKS)):
            raise ValueError('non-example IPv4 address in ' + name)


def heading_ids(data):
    found = set()
    for heading in re.findall(r'^#{1,6}\s+(.+?)\s*#*$', data.decode('utf-8'), re.M):
        slug = re.sub(r'[^\w\- ]', '', heading.lower()).replace(' ', '-')
        duplicate = slug
        index = 0
        while duplicate in found:
            index += 1
            duplicate = slug + '-' + str(index)
        found.add(duplicate)
    return found


def local_target(name, target):
    path, _, fragment = target.partition('#')
    # normpath is used only for checking the in-memory allowlist, never extraction.
    normalized = posixpath.normpath(posixpath.join(posixpath.dirname(name), unquote(path))) if path else name
    if normalized.startswith('../') or normalized.startswith('/'):
        raise ValueError('documentation link escapes export: ' + name)
    return normalized, unquote(fragment)


def rust_modules(name, data, files):
    """Check literal external modules in the reviewed top-level Rust layout.

    This is a bounded packaging check, not a Rust parser. Nested external modules
    and path overrides require deliberate support instead of guessing a target.
    Inline modules have no extra source file and are left to the compiler.
    """
    text = data.decode('utf-8')
    declarations = list(re.finditer(
        r'(?m)^([ \t]*)(?:pub(?:\([^\n)]*\))?[ \t]+)?mod[ \t]+([A-Za-z_][A-Za-z_0-9]*)[ \t]*;', text))
    if declarations and re.search(r'#\s*\[\s*path\s*=', text):
        raise ValueError('Rust path override requires explicit export review: ' + name)
    path = PurePosixPath(name)
    directory = path.parent if path.name in ('lib.rs', 'main.rs', 'mod.rs') else path.parent / path.stem
    checked = 0
    for declaration in declarations:
        if declaration.group(1):
            raise ValueError('nested external Rust module requires explicit export review: ' + name)
        module = declaration.group(2)
        candidates = (str(directory / (module + '.rs')), str(directory / module / 'mod.rs'))
        matches = [candidate for candidate in candidates if candidate in files]
        if len(matches) != 1:
            kind = 'missing' if not matches else 'ambiguous'
            raise ValueError(kind + ' Rust module dependency: ' + name + ' -> ' + module)
        checked += 1
    return checked


def validate_files(files, public=False):
    for name, data in files.items():
        safe_relative(name)
        if name not in ALLOWLIST:
            raise ValueError('file is outside exact export allowlist: ' + name)
        inspect_text(name, data)
    links = fixtures = modules = 0
    for name, data in files.items():
        if name.endswith('.md'):
            for _, target in LINK.findall(data.decode('utf-8')):
                if '://' in target or target.startswith('mailto:'):
                    continue
                destination, fragment = local_target(name, target.strip('<>'))
                if destination not in files:
                    raise ValueError('missing local documentation target: ' + name + ' -> ' + destination)
                if fragment and fragment not in heading_ids(files[destination]):
                    raise ValueError('missing documentation anchor: ' + name + ' -> ' + destination + '#' + fragment)
                links += 1
        if name.endswith('.rs'):
            modules += rust_modules(name, data, files)
            for fixture in re.findall(r'include(?:_str|_bytes)?!\s*\(\s*"([^"]+)"\s*\)', data.decode('utf-8')):
                destination, _ = local_target(name, fixture)
                if destination not in files:
                    raise ValueError('missing Rust include fixture: ' + destination)
                fixtures += 1
    if not re.search(r'^publish\s*=\s*false\s*$', files['Cargo.toml'].decode(), re.M):
        raise ValueError('crates.io publication must remain disabled')
    if set(files) != set(ALLOWLIST):
        raise ValueError('exact export allowlist is incomplete')
    if public:
        # Require explicit license fields in their real tables, not comments or
        # an unrelated table. This remains dependency-free on Python 3.10.
        def apache_metadata(data, table):
            section = re.search(
                r'(?ms)^[ \t]*\[' + re.escape(table)
                + r'\][ \t]*(?:#[^\n]*)?\n(.*?)(?=^[ \t]*\[|\Z)',
                data.decode('utf-8'))
            return section is not None and re.search(
                r"(?m)^[ \t]*license[ \t]*=[ \t]*([\"'])Apache-2\.0\1[ \t]*(?:#[^\n]*)?$",
                section.group(1)) is not None
        if not apache_metadata(files['Cargo.toml'], 'package') or not apache_metadata(
                files.get('python/pyproject.toml', b''), 'project'):
            raise ValueError('public candidate requires explicit Apache-2.0 package and SDK metadata')
        if not files.get('LICENSE') or files.get('python/LICENSE') != files['LICENSE']:
            raise ValueError('public candidate requires matching root and SDK licenses')
        if b'Apache License' not in files['LICENSE'] or b'Version 2.0' not in files['LICENSE']:
            raise ValueError('Apache license text is missing')
    return {'internal_documentation_links_checked': links, 'rust_include_fixtures_checked': fixtures,
            'rust_module_dependencies_checked': modules,
            # Legacy publication_disabled means Cargo registry publication only; it
            # does not forbid a separately authorized GitHub source release.
            'common_disclosure_checks_passed': True, 'publication_disabled': True,
            'crates_publication_disabled': True}


def fingerprint(data):
    return {'sha256': hashlib.sha256(data).hexdigest(), 'bytes': len(data)}


def pack(source, output, public=False):
    source, output = checked_home(source), checked_home(output)
    if source.is_symlink() or output.exists() or output.is_symlink():
        raise ValueError('source must be real; output must be new')
    original = {name: read_regular(source, name) for name in ALLOWLIST}
    files = {name: curate(name, data, public) for name, data in original.items()}
    checks = validate_files(files, public)
    for name, data in original.items():
        if read_regular(source, name) != data:
            raise ValueError('source changed during export: ' + name)
    manifest = {'schema_version': 1, 'classification': 'public_source_candidate_not_published' if public else 'private_source_review_not_publication',
                'files': {name: dict(fingerprint(files[name]), source_sha256=fingerprint(original[name])['sha256'],
                                    curated=files[name] != original[name]) for name in sorted(files)},
                'checks': checks,
                'exclusions': ['runtime', 'raw deployment evidence', 'credentials', 'work log',
                               'application source, models and datasets', 'private application Docker recipe'],
                'limits': ['Export does not perform publication', 'No new runtime validation',
                           'Disclosure checks are bounded hygiene checks, not a comprehensive security audit']}
    manifest_data = (json.dumps(manifest, sort_keys=True, indent=2) + '\n').encode()
    output.mkdir(parents=True, mode=0o700)
    tree = output / 'source'
    tree.mkdir(mode=0o700)
    for name, data in dict(files, **{'source-manifest.json': manifest_data}).items():
        path = tree / name
        path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        with path.open('xb') as stream:
            stream.write(data)
        path.chmod(0o600)
    archive = output / ('cedegrid-source.tar.gz' if public else 'resource-manager-source-review.tar.gz')
    archive_root = 'CedeGrid/' if public else 'ResourceManager/'
    with archive.open('xb') as stream, gzip.GzipFile(filename='', mode='wb', fileobj=stream, mtime=0) as gz:
        with tarfile.open(fileobj=gz, mode='w') as tar:
            for name, data in sorted(dict(files, **{'source-manifest.json': manifest_data}).items()):
                info = tarfile.TarInfo(archive_root + name)
                info.size = len(data); info.mode = 0o600; info.mtime = 0
                tar.addfile(info, io.BytesIO(data))
    archive.chmod(0o600)
    with tarfile.open(archive, 'r:gz') as tar:
        expected = {archive_root + name: data for name, data in dict(files, **{'source-manifest.json': manifest_data}).items()}
        members = tar.getmembers()
        if len(members) != len(expected) or {m.name for m in members} != set(expected):
            raise ValueError('archive membership mismatch')
        for member in members:
            if not member.isfile() or tar.extractfile(member).read() != expected[member.name]:
                raise ValueError('archive content mismatch')
    result = {'classification': manifest['classification'], 'archive': archive.name,
              'archive_sha256': fingerprint(archive.read_bytes())['sha256'], 'file_count': len(files),
              'archive_members': len(files) + 1, 'archive_roundtrip_verified': True,
              'curated_documents': [n for n in sorted(files) if files[n] != original[n]],
              'unchanged_core_sdk_and_lock_files': sum(n.startswith(('src/', 'python/resmgr/')) or n == 'Cargo.lock' for n in files),
              'checks': checks}
    (output / 'review.json').write_text(json.dumps(result, indent=2) + '\n')
    return result


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', required=True)
    parser.add_argument('--output', required=True, help='new private directory beneath your home')
    parser.add_argument('--public-candidate', action='store_true', help='export approved Apache-2.0 CedeGrid source; does not publish')
    args = parser.parse_args()
    print(json.dumps(pack(args.source, args.output, args.public_candidate), indent=2))
