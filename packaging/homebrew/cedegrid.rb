# The CedeGrid command-line tool from a GitHub Release, checked by each tarball's sha256.
# tools/build_packages.py writes the version and the sha256 values for each release candidate;
# the release carries the result as cedegrid.rb.
class Cedegrid < Formula
  desc "Run submitted compute jobs on spare CPU, RAM and GPU capacity"
  homepage "https://github.com/sidcatkr/CedeGrid"
  version "@VERSION@"
  license "Apache-2.0"

  on_macos do
    on_arm do
      url "@RELEASE_URL@/cedegrid-@VERSION@-darwin-arm64.tar.gz"
      sha256 "@SHA256_DARWIN_ARM64@"
    end
    on_intel do
      url "@RELEASE_URL@/cedegrid-@VERSION@-darwin-x64.tar.gz"
      sha256 "@SHA256_DARWIN_X64@"
    end
  end

  on_linux do
    on_arm do
      url "@RELEASE_URL@/cedegrid-@VERSION@-linux-arm64-gnu.tar.gz"
      sha256 "@SHA256_LINUX_ARM64_GNU@"
    end
    on_intel do
      url "@RELEASE_URL@/cedegrid-@VERSION@-linux-x64-gnu.tar.gz"
      sha256 "@SHA256_LINUX_X64_GNU@"
    end
  end

  def install
    bin.install "cedegrid"
    prefix.install "LICENSE", "NOTICE", "THIRD_PARTY_NOTICES.md"
  end

  test do
    assert_equal "cedegrid #{version}", shell_output("#{bin}/cedegrid --version").strip
  end
end
