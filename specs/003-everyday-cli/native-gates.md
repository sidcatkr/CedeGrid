# Native gates for 003

Passing tests on macOS does not establish Linux runtime behavior. Run these gates only on an
authorized Linux test host, and record each result here and in `docs/validation.md`. Any host
change a gate needs requires the user's approval first (FR-053). Evidence names no host,
account, address or private path; it uses the placeholders below.

## Environment

Every command assumes a checkout of the branch under test, as an existing non-root account:

```sh
mkdir -p "$HOME/.cache/cedegrid-tests"
export TMPDIR="$HOME/.cache/cedegrid-tests"
export CEDEGRID_TEST_PYTHON="$(command -v python3)"   # Python 3.10+
```

## Install on a clean account (SC-012, FR-073, T097)

The one-line install, run by a user with nothing installed: an empty `HOME` under an existing
account. A new account is used only with the user's approval. The gate never uses root or
`sudo`; the script's refusal to run as root is covered by `tests/test_install_script.py`.

### 1. Release files

For a published release, skip to step 2 and use its URL. Before publication, build the release
files from the checkout and serve them on loopback, which `install.sh` accepts over plain HTTP.
The binary comes from `tools/build_native.py`, as a release's does: a plain `cargo build` keeps
the builder's home path in the binary, and the release scan refuses it.

```sh
TARGET=linux-x64-gnu        # linux-arm64-gnu on aarch64
GATE=$(mktemp -d "$TMPDIR/install-gate.XXXXXX")
python3 tools/build_native.py --target "$TARGET" --out "$GATE/native"
python3 tools/build_packages.py --out "$GATE/candidate" --skip-python --skip-npm \
  --defer-manifest --binary "$TARGET=$GATE/native/cedegrid"
mkdir "$GATE/release"
cp "$GATE"/candidate/standalone/*.tar.gz "$GATE/candidate/install/install.sh" "$GATE/release/"
python3 -c 'import json,sys; sys.path.insert(0, "tools"); from release_manifest import checksums
m = json.load(open(sys.argv[1])); open(sys.argv[2], "w").write(checksums(m["artifacts"]))' \
  "$GATE/candidate/manifest.partial.json" "$GATE/release/SHA256SUMS"
(cd "$GATE/release" && exec python3 -m http.server 18731 --bind 127.0.0.1) &
SERVER=$!
VERSION=$(python3 tools/release_version.py)
URL=http://127.0.0.1:18731
```

### 2. Install with an empty HOME

```sh
CLEAN=$(mktemp -d "$TMPDIR/clean-home.XXXXXX")
curl -fsSL "$URL/install.sh" \
  | env -i HOME="$CLEAN" PATH=/usr/local/bin:/usr/bin:/bin CEDEGRID_RELEASE_URL="$URL" sh
echo "exit $?"
"$CLEAN/.local/bin/cedegrid" --version
stat -c '%a %U' "$CLEAN/.local/bin/cedegrid"
find "$CLEAN" -mindepth 1 | sort
```

For a published release, drop `CEDEGRID_RELEASE_URL` and fetch
`https://github.com/sidcatkr/CedeGrid/releases/download/vVERSION/install.sh`.

Pass: exit 0 and `Installed cedegrid VERSION into .../.local/bin/cedegrid.`; the script says
`~/.local/bin` is not on `PATH`; `--version` prints `cedegrid VERSION`; the binary is mode 755
and owned by the account; and the empty `HOME` now holds only `.local`, `.local/bin` and
`.local/bin/cedegrid`.

### 3. A changed tarball installs nothing

```sh
mkdir "$GATE/changed"
cp "$GATE/release/SHA256SUMS" "$GATE/release/install.sh" "$GATE/changed/"
python3 -c 'import sys; d = bytearray(open(sys.argv[1], "rb").read()); d[len(d) // 2] ^= 0xFF
open(sys.argv[2], "wb").write(d)' "$GATE/release/cedegrid-$VERSION-$TARGET.tar.gz" \
  "$GATE/changed/cedegrid-$VERSION-$TARGET.tar.gz"
(cd "$GATE/changed" && exec python3 -m http.server 18732 --bind 127.0.0.1) &
CHANGED=$!
CLEAN2=$(mktemp -d "$TMPDIR/clean-home.XXXXXX")
env -i HOME="$CLEAN2" PATH=/usr/local/bin:/usr/bin:/bin \
  CEDEGRID_RELEASE_URL=http://127.0.0.1:18732 sh "$GATE/changed/install.sh"
echo "exit $?"
find "$CLEAN2" -mindepth 1
kill "$CHANGED"
```

Pass: a nonzero exit; `cedegrid install: cedegrid-VERSION-TARGET.tar.gz does not match its
SHA256SUMS line; nothing was installed`; and the second `HOME` stays empty.

### 4. A first run

```sh
env -i HOME="$CLEAN" PATH="$CLEAN/.local/bin:/usr/local/bin:/usr/bin:/bin" \
  cedegrid run python3 -c 'print("first run")'
echo "exit $?"
kill "$SERVER"
```

Pass: `first run` and exit 0, with no step between the install and this command.

### Evidence

2026-11-26, the Linux x86_64 test host (kernel 7.0, glibc 2.43), an existing non-root account,
release files built from `f181efc` with `45119ad`'s strip fix, served on loopback:

| Step | Result | Verdict |
|---|---|---|
| 1. Release files | `build_native.py` built and stripped the binary with paths remapped (no home path left in it); `build_packages.py` packed the tarball and `install.sh`; `SHA256SUMS` named both by release name | pass, after `45119ad`: before it the strip step stopped on a missing LLVM library |
| 2. Empty `HOME` | exit 0, `Installed cedegrid 0.2.0`, the `PATH` hint, `cedegrid 0.2.0`, mode 755 owned by the account, and only `.local/bin/cedegrid` in the new `HOME` | pass |
| 3. Changed tarball | exit 1, `does not match its SHA256SUMS line; nothing was installed`, the second `HOME` empty | pass |
| 4. First run | `cedegrid run` is not in this build yet | waits for `run` |

SC-012 is not met until step 4 passes.
