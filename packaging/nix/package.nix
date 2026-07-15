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

  # Ship the udev rules so the NixOS module can install them via
  # services.udev.packages.
  postInstall = ''
    install -Dm444 udev/60-halod.rules \
      $out/lib/udev/rules.d/60-halod.rules
    install -Dm444 assets/dev.timp4w.Halod.desktop \
      $out/share/applications/dev.timp4w.Halod.desktop
    install -Dm444 assets/icon.svg \
      $out/share/icons/hicolor/scalable/apps/halod.svg
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
