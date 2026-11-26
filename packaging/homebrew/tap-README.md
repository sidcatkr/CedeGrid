# CedeGrid Homebrew tap

```sh
brew install sidcatkr/cedegrid/cedegrid
```

`Formula/cedegrid.rb` installs the CedeGrid command-line tool from its
[GitHub Release](https://github.com/sidcatkr/CedeGrid/releases), checking each tarball by its
sha256: on macOS 14 or newer, and on Linux x86_64 or aarch64 with glibc 2.35 or newer.

CedeGrid's release workflow writes the formula from the release's own `cedegrid.rb`, so a change
made here is replaced at the next release. Source, documentation and issues are in
[sidcatkr/CedeGrid](https://github.com/sidcatkr/CedeGrid). The formula is under the Apache
License 2.0, as CedeGrid is.
