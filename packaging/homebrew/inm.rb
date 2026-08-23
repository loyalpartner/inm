# Copy this into <your-tap-repo>/Formula/inm.rb (class name must match the
# filename in PascalCase — Inm for inm.rb).
#
# 🔴 Bump `url`/`sha256` by hand for each new release. Nothing in release.yml
# renders this the way it does packaging/aur/PKGBUILD.template, so it goes
# stale silently — it sat on v0.1.0 through two releases before anyone
# noticed, meaning `brew install` was shipping code two versions old.
#
# The sha256 is of the tag's GitHub-generated source tarball. Verified for
# v0.3.2 against both a fresh download and the checksum release.yml computed
# for the AUR package from the same tarball.
class Inm < Formula
  desc "Native manager for Incus virtual machines with the SPICE console embedded"
  homepage "https://github.com/loyalpartner/inm"
  url "https://github.com/loyalpartner/inm/archive/refs/tags/v0.3.2.tar.gz"
  sha256 "ba9e246e5c37d683187385a15943fa37b0f8d8275359997f1dac9f048b347511"
  license "MIT"

  depends_on "pkg-config" => :build
  depends_on "rust" => :build
  depends_on "spice-gtk"

  def install
    # Homebrew's spice-gtk .pc file isn't on the default pkg-config search
    # path, and its own prefix differs between Intel (/usr/local) and Apple
    # Silicon (/opt/homebrew) — asking the spice-gtk Formula for its own
    # opt_lib is the portable way to point at it instead of hardcoding either
    # prefix, unlike the manual `PKG_CONFIG_PATH=/opt/homebrew/...` in the
    # project's own README (written for Apple Silicon only).
    ENV.prepend_path "PKG_CONFIG_PATH", Formula["spice-gtk"].opt_lib/"pkgconfig"
    system "cargo", "install", *std_cargo_args
  end

  def caveats
    <<~EOS
      inm reuses the `incus` CLI's own configuration and credentials — it
      doesn't ask for its own. Run `incus remote add` first if you haven't:
        brew install incus
        incus remote add <name> https://<host>:8443
    EOS
  end

  test do
    # No CLI flags exist (it's a GUI app, `main()` just opens a window), so
    # there's nothing headless to actually exercise here — this only checks
    # the binary got installed and is executable, same as most GUI-only
    # formulas' tests.
    assert_predicate bin/"inm", :exist?
  end
end
