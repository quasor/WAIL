# WAIL Homebrew Formula
#
# This file is the source of truth for the Homebrew formula.
# It is copied automatically to the MostDistant/homebrew-wail tap on each release.
# The `url` and `sha256` fields below are updated by the release workflow.
#
# To install:
#   brew tap MostDistant/wail
#   brew install MostDistant/wail/wail

class Wail < Formula
  desc "Sync Ableton Link sessions across the internet with intervalic audio"
  homepage "https://github.com/MostDistant/WAIL"
  # url and sha256 are updated automatically by the release workflow
  url "https://github.com/MostDistant/WAIL/releases/download/v0.4.5/wail-0.4.5-src.tar.gz"
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"
  license "MIT"
  head "https://github.com/MostDistant/WAIL.git", branch: "main", submodules: true

  depends_on "cmake" => :build
  depends_on "pkg-config" => :build
  depends_on "rust" => :build
  depends_on "opus"
  depends_on :macos # requires macOS WebKit (used by Tauri)

  def install
    # Homebrew's superenv pkg-config shim references the legacy "pkg-config"
    # opt path, but modern Homebrew provides it via "pkgconf". Point the Rust
    # pkg-config crate directly to the real binary so audiopus_sys finds Opus.
    ENV["PKG_CONFIG"] = Formula["pkgconf"].opt_bin/"pkg-config"

    # CMake 4.x rejects old cmake_minimum_required() values in rusty_link's
    # vendored Ableton Link SDK. This env var tells CMake to accept them.
    ENV["CMAKE_POLICY_VERSION_MINIMUM"] = "3.5"

    # Build the main app binary.
    # Note: this produces the raw wail-tauri binary, not a full .app bundle.
    # For the polished macOS .app, use the DMG from the Releases page instead.
    system "cargo", "build", "--release", "--package", "wail-tauri", "--locked"
    bin.install "target/release/wail-tauri" => "wail"

    # Build plugin libraries first (separate --locked invocations, no nested cargo).
    system "cargo", "build", "--release", "--locked", "--package", "wail-plugin-send", "--lib"
    system "cargo", "build", "--release", "--locked", "--package", "wail-plugin-recv", "--lib"

    # Assemble CLAP/VST3 bundle directories from the pre-built dylibs (file ops only).
    system "cargo", "run", "--package", "xtask", "--release", "--locked", "--", "bundle-plugin", "--no-build"

    # Install plugin bundles to #{lib}. Run `wail-install-plugins` afterwards
    # to copy them to ~/Library/Audio/Plug-Ins/.
    (lib/"wail-plugin-send.clap").install Dir["target/bundled/wail-plugin-send.clap/*"]
    (lib/"wail-plugin-recv.clap").install Dir["target/bundled/wail-plugin-recv.clap/*"]
    (lib/"wail-plugin-send.vst3").install Dir["target/bundled/wail-plugin-send.vst3/*"]
    (lib/"wail-plugin-recv.vst3").install Dir["target/bundled/wail-plugin-recv.vst3/*"]

    # Install the plugin installation helper script (useful for manual reinstall).
    bin.install "scripts/wail-install-plugins.sh" => "wail-install-plugins"
  end

  def caveats
    <<~EOS
      To install the CLAP and VST3 plugins to your DAW's plugin directories, run:
        wail-install-plugins

      This copies plugin bundles to:
        ~/Library/Audio/Plug-Ins/CLAP/
        ~/Library/Audio/Plug-Ins/VST3/

      Rescan plugins in your DAW to pick them up.

      Note: `wail` launches the app binary directly. For the polished macOS .app
      bundle (dock icon, native menu bar), download the DMG from:
        https://github.com/MostDistant/WAIL/releases
    EOS
  end

  test do
    assert_predicate bin/"wail", :exist?
    assert_predicate bin/"wail-install-plugins", :exist?
    assert_predicate lib/"wail-plugin-send.clap", :exist?
    assert_predicate lib/"wail-plugin-recv.clap", :exist?
    assert_predicate lib/"wail-plugin-send.vst3", :exist?
    assert_predicate lib/"wail-plugin-recv.vst3", :exist?
  end
end
