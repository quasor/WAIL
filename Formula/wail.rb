class Wail < Formula
  desc "Sync Ableton Link sessions across the internet via WebRTC"
  homepage "https://github.com/quasor/WAIL"
  url "https://github.com/quasor/WAIL/archive/refs/tags/v0.4.5.tar.gz"
  sha256 "PLACEHOLDER"
  license "MIT"
  head "https://github.com/quasor/WAIL.git", branch: "main"

  depends_on "cmake" => :build
  depends_on "pkg-config" => :build
  depends_on "rust" => :build
  depends_on "opus"
  depends_on :macos

  def install
    # cargo-nih-plug is needed to bundle CLAP/VST3 plugins.
    # Install it as a local build tool (not system-wide).
    system "cargo", "install", "--git",
           "https://github.com/robbert-vdh/nih-plug.git",
           "cargo-nih-plug",
           "--root", buildpath/"tools"
    ENV.prepend_path "PATH", buildpath/"tools/bin"

    # Build the Tauri desktop app binary.
    # The frontend (HTML/JS/CSS) is embedded at compile time via tauri_build.
    system "cargo", "build", "--release", "-p", "wail-tauri"

    # Build CLAP and VST3 plugin bundles.
    system "cargo", "nih-plug", "bundle", "wail-plugin-send", "--release"
    system "cargo", "nih-plug", "bundle", "wail-plugin-recv", "--release"

    # --- Install the macOS .app bundle ---
    app_contents = prefix/"WAIL.app/Contents"
    (app_contents/"MacOS").mkpath
    (app_contents/"MacOS").install "target/release/wail-tauri" => "WAIL"
    (app_contents/"Resources").mkpath
    (app_contents/"Resources").install "crates/wail-tauri/icons/icon.icns"
    (app_contents/"Info.plist").write <<~PLIST
      <?xml version="1.0" encoding="UTF-8"?>
      <!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
        "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
      <plist version="1.0">
      <dict>
        <key>CFBundleDisplayName</key>
        <string>WAIL</string>
        <key>CFBundleExecutable</key>
        <string>WAIL</string>
        <key>CFBundleIconFile</key>
        <string>icon</string>
        <key>CFBundleIdentifier</key>
        <string>com.wail.desktop</string>
        <key>CFBundleName</key>
        <string>WAIL</string>
        <key>CFBundlePackageType</key>
        <string>APPL</string>
        <key>CFBundleShortVersionString</key>
        <string>#{version}</string>
        <key>CFBundleVersion</key>
        <string>#{version}</string>
        <key>NSHighResolutionCapable</key>
        <true/>
        <key>NSMicrophoneUsageDescription</key>
        <string>WAIL needs microphone access for audio capture.</string>
      </dict>
      </plist>
    PLIST

    # Symlink the binary into Homebrew's bin so `wail` works from terminal
    bin.install_symlink app_contents/"MacOS/WAIL" => "wail"

    # --- Install audio plugins ---
    (lib/"clap").install "target/bundled/wail-plugin-send.clap"
    (lib/"clap").install "target/bundled/wail-plugin-recv.clap"
    (lib/"vst3").install "target/bundled/wail-plugin-send.vst3"
    (lib/"vst3").install "target/bundled/wail-plugin-recv.vst3"
  end

  def post_install
    # Symlink plugins into the standard macOS audio plugin directories
    # so DAWs can discover them without manual setup.
    clap_dir = Pathname.new(Dir.home)/"Library/Audio/Plug-Ins/CLAP"
    vst3_dir = Pathname.new(Dir.home)/"Library/Audio/Plug-Ins/VST3"
    clap_dir.mkpath
    vst3_dir.mkpath

    %w[wail-plugin-send wail-plugin-recv].each do |plugin|
      clap_src = lib/"clap/#{plugin}.clap"
      clap_dst = clap_dir/"#{plugin}.clap"
      clap_dst.unlink if clap_dst.symlink?
      clap_dst.make_symlink(clap_src) if clap_src.exist?

      vst3_src = lib/"vst3/#{plugin}.vst3"
      vst3_dst = vst3_dir/"#{plugin}.vst3"
      FileUtils.rm_rf(vst3_dst) if vst3_dst.exist? || vst3_dst.symlink?
      vst3_dst.make_symlink(vst3_src) if vst3_src.exist?
    end

    # Optionally link the .app into ~/Applications for Finder/Spotlight access
    apps_dir = Pathname.new(Dir.home)/"Applications"
    apps_dir.mkpath
    app_link = apps_dir/"WAIL.app"
    app_link.unlink if app_link.symlink?
    app_link.make_symlink(prefix/"WAIL.app") if (prefix/"WAIL.app").exist?
  end

  def caveats
    <<~EOS
      WAIL has been installed!

      Launch from terminal:
        wail

      Or open from Finder:
        ~/Applications/WAIL.app

      Audio plugins (CLAP + VST3) have been symlinked to:
        ~/Library/Audio/Plug-Ins/CLAP/
        ~/Library/Audio/Plug-Ins/VST3/

      Rescan plugins in your DAW to detect WAIL Send and WAIL Recv.

      Note: Building from source avoids macOS Gatekeeper quarantine.
      No `xattr -d` workaround needed.
    EOS
  end

  test do
    assert_predicate bin/"wail", :exist?
    assert_predicate bin/"wail", :executable?
    assert_predicate lib/"clap/wail-plugin-send.clap", :exist?
    assert_predicate lib/"vst3/wail-plugin-recv.vst3", :exist?
  end
end
