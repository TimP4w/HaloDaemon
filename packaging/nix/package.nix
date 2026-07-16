# The HaloDaemon package — builds halod and halod-gui from the cargo workspace.
# Called from the root flake with the flake root as `src` and the flake
# revision as `buildHash`.
{
  pkgs,
  lib,
  version,
  buildHash,
  src,
}:
let
  # Updated by the release action together with the daemon version. This is
  # deliberately part of the existing package derivation: the NixOS module's
  # default package therefore embeds the same official plugin revision without
  # an extra option or a networked build step.
  officialPlugins = pkgs.fetchFromGitHub {
    owner = "TimP4w";
    repo = "HaloDaemon-plugins";
    rev = "2769e9ceff857771f045ba28ebfe30abd07497c7";
    hash = "sha256-vQHGjkBWSobz0Q1lH+T9iMXMELJHLomJxpNQhVtafeA=";
  };
in
pkgs.rustPlatform.buildRustPackage {
  pname = "halod";
  inherit version;

  # Only what cargo needs — keeps build artifacts and .git out of the
  # Nix store and off every evaluation.
  src = lib.cleanSourceWith {
    inherit src;
    filter =
      path: type:
      let
        base = baseNameOf path;
      in
      base != "target" && base != ".git" && base != "result";
  };

  # The cargo workspace lives in src/, not at the repo root.
  # cargoRoot: where the cargo setup hook finds Cargo.lock / Cargo.toml.
  # buildAndTestSubdir: where the build & test phases run.
  cargoRoot = "src";
  buildAndTestSubdir = "src";

  # `.git` is stripped from the source above, so the UI build script
  # can't derive the commit itself — hand it the flake revision.
  HALOD_BUILD_HASH = buildHash;

  cargoLock = {
    lockFile = src + "/src/Cargo.lock";
    outputHashes = {
      "q565-0.4.0" = "sha256-3DHMdzWa1e+51VGLp5Q/bFsvNfNssrDG9RteWm5Z7lA=";
    };
  };

  nativeBuildInputs = with pkgs; [
    pkg-config
    makeWrapper
    rustPlatform.bindgenHook
    cargo-about
    perl # git2 needs perl for vendored-openssl
  ];

  buildInputs = with pkgs; [
    hidapi
    libusb1
    pipewire
    ffmpeg
    udev
    wayland
    libxkbcommon
    libGL
    dbus
  ];

  # The GUI / integration tests need a display and hardware; CI runs
  # `cargo test -p halod` separately.
  doCheck = false;

  # Generate the exact same embedded payload inside the sandbox from the
  # fixed-output plugin source. No network is available or needed here.
  preBuild = ''
    cargo build -p halod-plugin-signing
    mkdir -p plugin-bundle
    target/debug/halod-plugin-signing verify ${officialPlugins} \
      --trusted-key halodaemon-official-2026=tjbwm5X4f70e+soVNV1AfRyb/TtnEsNNl+93YMO6IhQ=
    target/debug/halod-plugin-signing bundle ${officialPlugins} \
      --commit 2769e9ceff857771f045ba28ebfe30abd07497c7 \
      --output plugin-bundle/official-plugins.bundle
    {
      printf 'Official HaloDaemon plugin snapshot\nCommit: %s\n\n' \
        2769e9ceff857771f045ba28ebfe30abd07497c7
      find ${officialPlugins}/LICENSES -maxdepth 1 -type f -print0 | sort -z | xargs -0 -r cat
    } > plugin-bundle/official-plugins-licenses.txt
    export HALOD_REQUIRE_PLUGIN_BUNDLE=1
    export HALOD_OFFICIAL_PLUGIN_BUNDLE=$PWD/plugin-bundle/official-plugins.bundle
    export HALOD_OFFICIAL_PLUGIN_LICENSES=$PWD/plugin-bundle/official-plugins-licenses.txt
  '';

  postBuild = ''
    target/release/halod udev-rules --embedded \
      > plugin-bundle/60-halod.rules
  '';

  # Ship the udev rules so the NixOS module can install them via
  # services.udev.packages.
  postInstall = ''
    install -Dm444 plugin-bundle/60-halod.rules \
      $out/lib/udev/rules.d/60-halod.rules
    install -Dm444 assets/dev.timp4w.Halod.desktop \
      $out/share/applications/dev.timp4w.Halod.desktop
    install -Dm444 assets/icon.svg \
      $out/share/icons/hicolor/scalable/apps/halod.svg
    install -Dm444 plugin-bundle/official-plugins-licenses.txt \
      $out/share/licenses/halod/plugins/NOTICE.txt
    install -Dm444 ${officialPlugins}/REUSE.toml \
      $out/share/licenses/halod/plugins/REUSE.toml
    for license in ${officialPlugins}/LICENSES/*; do
      install -Dm444 "$license" "$out/share/licenses/halod/plugins/$(basename "$license")"
    done
  '';

  # eframe/wgpu dlopens libGL and libwayland at runtime; wrap the binary
  # so those libraries are findable in the Nix store.
  postFixup = ''
    wrapProgram $out/bin/halod-gui \
      --prefix LD_LIBRARY_PATH : ${
        lib.makeLibraryPath (
          with pkgs;
          [
            libGL
            wayland
            libxkbcommon
          ]
        )
      }
    # The LCD video engine spawns `ffmpeg` from PATH at runtime.
    wrapProgram $out/bin/halod \
      --prefix PATH : ${lib.makeBinPath [ pkgs.ffmpeg ]}
  '';

  meta = {
    description = "Peripheral control daemon (fan curves, RGB, LCD, audio EQ, DPI)";
    homepage = "https://github.com/TimP4w/HaloDaemon";
    license = lib.licenses.gpl3Plus;
    platforms = lib.platforms.linux;
    mainProgram = "halod-gui";
  };
}
