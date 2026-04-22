# Homebrew formula for ccaudit.
#
# This file is not used from the main repo directly. Copy it into a tap
# repository (e.g. `electricapp/homebrew-tap`) after the first release
# and update the `url` + `sha256` pairs to point at the GitHub release
# artifacts.
#
# Once the tap exists:
#   brew tap electricapp/tap
#   brew install ccaudit
#
# TODO(pre-launch):
#   - Create the tap repo under the GitHub org
#   - Upload release binaries via the release workflow
#   - Paste real sha256 sums below (placeholders abort `brew install`)

class Ccaudit < Formula
  desc "Fast Claude Code log viewer — CLI, TUI, and web dashboard"
  homepage "https://github.com/electricapp/ccaudit"
  version "0.1.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/electricapp/ccaudit/releases/download/v#{version}/ccaudit-aarch64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_DARWIN_ARM64_SHA256"
    end
    on_intel do
      url "https://github.com/electricapp/ccaudit/releases/download/v#{version}/ccaudit-x86_64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_DARWIN_X64_SHA256"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/electricapp/ccaudit/releases/download/v#{version}/ccaudit-aarch64-unknown-linux-musl.tar.gz"
      sha256 "REPLACE_WITH_LINUX_ARM64_SHA256"
    end
    on_intel do
      url "https://github.com/electricapp/ccaudit/releases/download/v#{version}/ccaudit-x86_64-unknown-linux-musl.tar.gz"
      sha256 "REPLACE_WITH_LINUX_X64_SHA256"
    end
  end

  def install
    bin.install "ccaudit"
  end

  test do
    assert_match "ccaudit", shell_output("#{bin}/ccaudit --help")
  end
end
