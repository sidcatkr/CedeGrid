# CedeGrid 0.2.0 release

CedeGrid 0.2.0 contains a standalone native CLI, a TypeScript SDK and launcher in
npm `cedegrid`, and a Python-only SDK in PyPI `cedegrid`. The executable, Rust
library, Python import, and environment prefix are `cedegrid`/`CEDEGRID_` without
old-name aliases. Runtime configuration is TOML 1.0. Rust 1.88, Python 3.10, and
Node 22.14 are the compatibility floors.

Version 0.2.0 is published on npm, PyPI, and GitHub using the original checked
archives. Publication was explicitly requested before qualification finished;
the [qualification manifest](release-0.2-gates.json) retains incomplete and failed
checks. The Windows native npm package was rejected by npm's spam filter.
Fresh public-registry installation passed on macOS arm64 with npm scripts
disabled. The release used the owner's npm login and PyPI token, without an
OIDC provenance attestation. Crates.io publication remains disabled.

## Artifacts and support boundary

The native package targets are `linux-x64-gnu`, `linux-arm64-gnu`, `darwin-x64`,
`darwin-arm64`, and `win32-x64`. GNU binaries target glibc 2.35 and Linux kernel
5.15. macOS deployment target is 14. Windows uses the MSVC x64 client target and
requires Windows 11 24H2 qualification. Linux/macOS execute supported workloads;
Windows refuses execution/recovery before state initialization. Standalone
binaries require neither Python nor Node.

macOS binaries have an ad-hoc integrity signature after stripping build provenance
paths. They are not Developer ID signed or notarized. Windows binaries are not
Authenticode signed. Checksums and build attestations identify release files;
no certificate-backed platform-signing claim is made.

The npm main tarball contains compiled ESM/CommonJS, matching declarations,
TypeScript-derived launcher code, and exact-version optional native dependencies.
Installation with scripts disabled requires no build or download hook. The SDK
imports independently of native CLI resolution. Python provides an original wheel
and source distribution, plus a separate rebuilt-sdist install check.

## Build and validate locally

`tools/build_native.py` produces immutable native candidates and their build
records; it remaps private paths, strips debug records and hashes the final
binary. Linux cross builds use Zig with a glibc 2.35 sysroot. ARM64 links preserve
the Cortex-A53 erratum workaround through LLVM. Windows cross builds may use
cargo-xwin with an isolated SDK/CRT cache. Native execution on the minimum OS is
still required after cross compilation.

`tools/build_packages.py --out NEW_DIRECTORY --binary TARGET=PATH ...` builds
wheel/sdist, npm platform packages, the main npm tarball, and standalone archives.
The full five-target set is required. It verifies embedded binary bytes and scans
extracted archive members, including printable native strings, for accidental
private data. Preserve failed candidates and create a new candidate after fixes;
resume only an unfinished package set without rewriting its existing files.

The npm verification harness runs a disposable loopback registry without uplinks,
serving the original unpublished tarballs with exact optional-dependency versions.
Clean local/global, scripts-disabled, optional-omitted, ESM/CJS, declarations,
transport, native self-launch, and terminal signal paths are tested outside the
checkout. Python tests install the original wheel and independently rebuilt sdist
across 3.10–3.14. Cross-SDK workers pass metadata through native publication and
SQLite, restart the coordinator, and verify with the other installed SDK.

`tools/release_manifest.py` freezes the reviewed source file hash map, checks each
artifact hash, and enforces the fixed gate set. Qualification records are excluded
from their own source digest. Its `--require-stable` mode fails for missing,
changed, unqualified, or mismatched evidence. The [detailed contract](release-0.2-spec.md)
defines descriptor/quota faults, namespace interleavings, 100k/1m pagination,
three-host work, both GPUs, and three-agent 256 MiB artifact/control latency gates.

## Publication prerequisites and exact-byte workflow

The public registry preflight found no existing project for the main npm name,
its five platform names, or PyPI `cedegrid`. Absence does not reserve a name or
prove permission to publish. Each project's owner and trusted publisher must be
configured and verified externally. A PyPI pending publisher does not reserve
its project name, and npm `whoami` does not validate OIDC publication.

The manually dispatched release workflow consumes a previously qualified artifact
set. It rechecks source and file hashes and every stable gate, then publishes native
npm packages, the original Python files, and finally the main npm package. Existing
registry files are skipped only after their cryptographic digest matches. A
conflicting existing version aborts. Publish jobs never rebuild or repackage.
The default workflow dispatch performs verification only; upload requires its
explicit publish input and the repository's release environment.

Run `release-candidate.yml` in `build` mode to produce the original unqualified
package set. After qualification against those exact files, its `accept-qualified`
mode verifies the prepared bundle on the protected `cedegrid-release-qualification`
runner. Set the environment variable `QUALIFIED_CANDIDATE_DIRECTORY` through the
same-named repository/environment Actions variable to that bundle's directory.
Only reviewed manifest members and hashed public evidence enter the resulting
`cedegrid-qualified-0.2.0` artifact; publishing consumes that completed run ID.
The build matrix is for compilation. Dedicated baseline and physical-host evidence
is still required, following [GitHub's runner specifications](https://docs.github.com/en/actions/reference/runners/github-hosted-runners).

The prepared qualified-release workflow uses GitHub-hosted OIDC, Node 24.14.0 and npm 11.16.0, following
[npm trusted publishing](https://docs.npmjs.com/trusted-publishers/) and
[PyPI trusted publishing](https://packaging.python.org/en/latest/guides/publishing-package-distribution-releases-using-github-actions-ci-cd-workflows/).
Configure the exact repository, release workflow filename, environment, and
allowed publisher action for every registry project. Current npm publishers may
default to staged publication; explicitly permit direct `npm publish` for this
workflow. Actual authentication/publication can be recorded as PASS only after a
real upload. GitHub artifact attestations follow the
[official provenance guidance](https://docs.github.com/en/actions/how-tos/secure-your-work/use-artifact-attestations/use-artifact-attestations).

## Migration and retained limits

Drain and stop the old services, supervisors and workload writers before upgrade.
The [migration guide](migration-0.2.md) explains effective-path-preserving config
conversion, explicit offline state upgrade, immutable rollback bundles, and held
legacy work. Mixed-version rolling upgrade and in-place downgrade are unsupported.

GPU sharing remains best effort. Hostile-code isolation, arbitrary unmediated
process descendants, hard VRAM partitions, and unqualified optional controls are
outside the stable claim. Preserve [LICENSE](../LICENSE), [NOTICE](../NOTICE), and
collected dependency license texts. NVIDIA driver libraries, application assets,
credentials, private host details, and raw deployment evidence are excluded from
public artifacts. See [guarantees](guarantees.md).
