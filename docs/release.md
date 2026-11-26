# CedeGrid release

From 0.3.0 on, CedeGrid ships as files on GitHub Releases only. The command-line tool installs
for one user with a script, and the Python and TypeScript SDKs are files attached to the same
release. Nothing is published to npm or PyPI. Version 0.2.0 stays on both registries, and
[Moving off npm and PyPI](migration-registries.md) covers an existing install. The 0.2.0
[qualification record](release-0.2-gates.json), [evidence](release-0.2-evidence.md) and
[contract](release-0.2-spec.md) are kept as they were.

The executable, Rust library, Python import and environment prefix are `cedegrid`/`CEDEGRID_`.
Runtime configuration is TOML 1.0. Rust 1.88, Python 3.10 and Node 22.14 are the compatibility
floors.

## Version

The release version is the `version` in Cargo.toml's `[package]` table. Every release tool and
both release workflows read it through `tools/release_version.py`; nothing else names it.
`python3 tools/release_version.py --check` prints it, and fails unless `python/pyproject.toml`
and `npm/package.json` carry the same version. To start a release, change those three lines
together and run `cargo check` so Cargo.lock follows. The release's gates file is
`docs/release-MAJOR.MINOR-gates.json`, here [release-0.3-gates.json](release-0.3-gates.json).

## Release files

| File | What it holds |
| --- | --- |
| `install.sh` | The user-level installer, with the release version written in |
| `cedegrid.rb` | The Homebrew formula, with each macOS and Linux tarball's sha256 written in |
| `cedegrid-VERSION-linux-x64-gnu.tar.gz`, `cedegrid-VERSION-linux-arm64-gnu.tar.gz` | The CLI for Linux, glibc 2.35 and kernel 5.15 or newer |
| `cedegrid-VERSION-darwin-arm64.tar.gz`, `cedegrid-VERSION-darwin-x64.tar.gz` | The CLI for macOS 14 or newer |
| `cedegrid-VERSION-win32-x64.zip` | The Windows client, which refuses to run work |
| `cedegrid_VERSION_amd64.deb`, `cedegrid_VERSION_arm64.deb`, `cedegrid-system_VERSION_*.deb` | System mode packages, for administrators |
| `cedegrid-VERSION-py3-none-any.whl`, `cedegrid-VERSION.tar.gz` | The Python SDK, wheel and sdist |
| `cedegrid-VERSION.tgz` | The TypeScript SDK |
| `cedegrid-VERSION-source.tar.gz` | The reviewed source export |
| `SHA256SUMS` | One line for each file above, under the file's release name |

Each CLI archive holds one directory, `cedegrid-VERSION-TARGET/`, with the binary, LICENSE,
NOTICE and THIRD_PARTY_NOTICES.md. Because `SHA256SUMS` names each file as the release does,
`sha256sum -c --ignore-missing SHA256SUMS` works in a download directory, and `install.sh` finds
its tarball's line there.

The wheel holds the `cedegrid` package's Python files and metadata only, and the TypeScript SDK
tarball has no `bin` entry, no optional native packages and no install hook. Neither installs a
command; the CLI is its own file.

macOS binaries have an ad-hoc integrity signature after stripping build provenance paths. They
are not Developer ID signed or notarized. Windows binaries are not Authenticode signed.
Checksums and build attestations identify release files; no certificate-backed platform-signing
claim is made.

## Build and validate locally

`tools/build_native.py` produces immutable native candidates and their build records; it
remaps private paths, strips debug records and hashes the final binary. Linux cross builds use
Zig with a glibc 2.35 sysroot. ARM64 links preserve the Cortex-A53 erratum workaround through
LLVM. Windows cross builds may use cargo-xwin with an isolated SDK/CRT cache. Native execution
on the minimum OS is still required after cross compilation.

`tools/build_packages.py --out NEW_DIRECTORY --binary TARGET=PATH ...` builds the wheel and
sdist, the TypeScript SDK tarball, one standalone archive per target, `install.sh` with the
version written in, and `cedegrid.rb` from [the formula template](../packaging/homebrew/cedegrid.rb)
once all four macOS and Linux archives exist; `tools/release_manifest.py` checks that the formula
names exactly those archives with their sha256. It refuses an SDK tarball that declares a command, an optional native
package or an install hook, verifies embedded binary bytes, and scans extracted archive members,
including printable native strings, for accidental private data. The candidate job then builds
the System mode Debian packages, `cedegrid` and `cedegrid-system` for amd64 and arm64, with
`tools/build_deb.py` from the same two Linux binaries. Preserve failed candidates and create a
new candidate after fixes; resume only an unfinished package set without rewriting its files.

`tools/verify_python_packages.py` installs the original wheel and an independently rebuilt
sdist on Python 3.10 to 3.14, and refuses a wheel that holds more than the SDK.
`tools/verify_package_interop.py` installs the TypeScript SDK from the candidate's tarball and
unpacks the native binary from its standalone archive, then passes metadata from workers of
both SDKs through native publication and SQLite, restarts the coordinator, and reads it back
with the other SDK. `tests/test_install_script.py` runs `install.sh` against release files
served on loopback.

`tools/release_manifest.py` freezes the reviewed source file hash map, writes `SHA256SUMS`,
checks each artifact hash and enforces the fixed gate set. Qualification records are excluded
from their own source digest. Its `--require-stable` mode fails for missing, changed,
unqualified or mismatched evidence. The 0.3 gate set drops 0.2's two registry gates and adds
`SDK-FILES` (the wheel, sdist and TypeScript SDK tarball, and that they hold the SDK only) and
`INSTALL-SCRIPT` (003 SC-012: `install.sh` on a clean account without root installs the pinned
version, and installs nothing from a changed tarball). The other gates keep the scenarios
[the 0.2 contract](release-0.2-spec.md) defines.

Before a merge into `main`, a release or a tag, one more gate applies: an independent security
review of the commit to be published, run in an isolated environment. Nothing is merged into
`main`, released or tagged until it has run on that exact commit and its findings are fixed or
accepted in writing.

## Publishing

Run `release-candidate.yml` in `build` mode to produce the original unqualified file set,
uploaded as `cedegrid-unqualified-VERSION`. After qualification against those exact files, its
`accept-qualified` mode verifies the prepared bundle on the protected
`cedegrid-release-qualification` runner. Set the Actions variable
`QUALIFIED_CANDIDATE_DIRECTORY` to that bundle's directory. Only reviewed manifest members and
hashed public evidence enter the resulting `cedegrid-qualified-VERSION` artifact. The build
matrix is for compilation; dedicated baseline and physical-host evidence is still required,
following [GitHub's runner specifications](https://docs.github.com/en/actions/reference/runners/github-hosted-runners).

`release.yml` takes that run's ID. It rechecks the source, every file hash and every stable
gate, and checks that `SHA256SUMS` matches the manifest. With its `publish` input set, in the
repository's release environment, it attests the files and creates the GitHub Release
`vVERSION` with them and `SHA256SUMS`, unchanged. It never rebuilds or repackages, uses no
registry credentials, and stops without overwriting anything when the release already exists.
The default dispatch verifies only. Attestations follow the
[official provenance guidance](https://docs.github.com/en/actions/how-tos/secure-your-work/use-artifact-attestations/use-artifact-attestations).

Two second channels install the same version. After the release job, the `homebrew-tap` job
downloads the published `cedegrid.rb` and `SHA256SUMS`, checks the formula with
`tools/update_homebrew_tap.py`, and pushes it to the tap
[sidcatkr/homebrew-cedegrid](https://github.com/sidcatkr/homebrew-cedegrid) as
`Formula/cedegrid.rb`, with the tap's README from `packaging/homebrew/tap-README.md`; that is what
`brew install sidcatkr/cedegrid/cedegrid` installs. It needs the `HOMEBREW_TAP_TOKEN` secret in
the release environment: a fine-grained token for that repository alone, with read and write
access to its contents. It is a job of its own, so it can be re-run alone. crates.io gets this
commit's crate for `cargo install --locked cedegrid` when the `publish_crate` input is also set,
after the GitHub Release exists; it needs the `CARGO_REGISTRY_TOKEN` secret in the release
environment and the crate name on crates.io.

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
