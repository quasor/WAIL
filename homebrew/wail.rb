# WAIL Homebrew Formula
#
# This file is the source of truth for the Homebrew formula.
# It is copied automatically to the MostDistant/homebrew-wail tap on each release.
# The `url` and `sha256` fields below are updated by the release workflow.
#
# Architecture: Go/Wails desktop app + Rust CLAP/VST3 plugins.
# The app (session orchestration, Link sync, signaling) is built with Go.
# The audio plugins (Opus encode/decode, DAW integration) remain in Rust.
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
  depends_on "go" => :build
  depends_on "pkg-config" => :build
  depends_on "rust" => :build # needed for CLAP/VST3 plugins only
  depends_on "opus"
  depends_on "opusfile"
  depends_on "rtmidi"
  depends_on :macos # requires macOS WebKit (used by Wails webview)

  def install
    # Homebrew's superenv pkg-config shim references the legacy "pkg-config"
    # opt path, but modern Homebrew provides it via "pkgconf". Point the Rust
    # pkg-config crate directly to the real binary so audiopus_sys finds Opus.
    ENV["PKG_CONFIG"] = Formula["pkgconf"].opt_bin/"pkg-config"

    # CMake 4.x rejects old cmake_minimum_required() values in the Ableton
    # Link SDK. This env var tells CMake to accept them.
    ENV["CMAKE_POLICY_VERSION_MINIMUM"] = "3.5"

    # --- Go app (session orchestration, Link sync, signaling) ---

    # Clone and build the abletonlink-go CGo dependency (Ableton Link 4 SDK).
    # The package bundles Link as a git submodule and builds via CMake.
    system "git", "clone", "--recursive",
           "https://github.com/DatanoiseTV/abletonlink-go.git",
           buildpath/"abletonlink-go-build"
    system "bash", (buildpath/"abletonlink-go-build/build.sh").to_s

    # Build the Go/Wails desktop app.
    cd "wail-app" do
      # Point the go.mod replace directive to the local abletonlink-go clone.
      inreplace "go.mod",
        /replace github\.com\/DatanoiseTV\/abletonlink-go\s*=>\s*.*/,
        "replace github.com/DatanoiseTV/abletonlink-go => #{buildpath}/abletonlink-go-build"
      system "go", "build", "-o", "wail", "."
    end
    bin.install "wail-app/wail"

    # --- Rust plugins (CLAP/VST3 audio plugins for DAW integration) ---

    # Build plugin libraries (separate invocations, no nested cargo).
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
