# Moving off npm and PyPI

CedeGrid 0.2.0 is the last version published to npm and PyPI. Later versions ship only as files
on [GitHub Releases](https://github.com/sidcatkr/CedeGrid/releases), each listed with its sha256
in the release's `SHA256SUMS`. One install channel that checks the download needs no registry
account, and the SDKs keep their APIs (003 FR-073, FR-080).

## What stays as it is

- `cedegrid@0.2.0` on npm, its native packages, and `cedegrid==0.2.0` on PyPI stay where they
  are. Nothing is unpublished. Installs that pin 0.2.0 keep resolving.
- A CLI installed with `npm install --global cedegrid@0.2.0` keeps running 0.2.0. It does not
  update itself.
- Configuration files with `config_version = 1` load in later versions unchanged.

## Moving the CLI to a later version

1. Remove the npm-installed launcher, so it does not shadow the new binary on `PATH`:

   ```sh
   npm uninstall --global cedegrid
   ```

2. Install the release you want for your user:

   ```sh
   curl -fsSL https://github.com/sidcatkr/CedeGrid/releases/download/v<version>/install.sh | sh
   ```

   The script installs into `~/.local/bin/cedegrid` and checks the download against
   `SHA256SUMS` first. `brew install sidcatkr/cedegrid/cedegrid` and `cargo install --locked
   cedegrid` install the same version.

3. Check that the new binary is the one found: `command -v cedegrid` should print
   `~/.local/bin/cedegrid`, and `cedegrid --version` the release's version.

## Moving the SDKs

Download the wheel or the npm tarball from the release, check them against `SHA256SUMS`, and
install from the files:

```sh
sha256sum --check --ignore-missing SHA256SUMS
python -m pip install ./cedegrid-<version>-py3-none-any.whl
npm install ./cedegrid-<version>.tgz
```

A project that lists `cedegrid` from a registry pins `0.2.0`, or points at the release file:
for pip, a requirement such as `cedegrid @ https://github.com/sidcatkr/CedeGrid/releases/download/v<version>/cedegrid-<version>-py3-none-any.whl`
with its hash; for npm, the tarball's URL as the dependency's version.

## On the registries

Deprecating or removing anything on npm or PyPI is the project owner's step, done by hand. A
deprecation message for npm can point here:

```sh
npm deprecate cedegrid@"<=0.2.0" "Later versions ship on GitHub Releases: https://github.com/sidcatkr/CedeGrid/blob/main/docs/migration-registries.md"
```
