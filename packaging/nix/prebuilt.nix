{
  pkgs,
  lib,
  src,
}:
let
  release = "v0.11.4";
  tarballHash = "sha256-fvsrBdsWYThGS+KHWcg9qDh/UXPGW6Kcu7chUm5MW90=";

  assets = builtins.path {
    path = src + "/assets";
    name = "halod-assets";
  };
in
pkgs.stdenv.mkDerivation {
  pname = "halod";
  version = lib.removePrefix "v" release;

  src = pkgs.fetchurl {
    url = "https://github.com/TimP4w/HaloDaemon/releases/download/${release}/halod-linux-x64.tar.gz";
    hash = tarballHash;
  };

  nativeBuildInputs = with pkgs; [
    autoPatchelfHook
    makeWrapper
  ];

  buildInputs = with pkgs; [
    stdenv.cc.cc.lib
    udev
    libusb1
    pipewire
  ];

  installPhase = ''
    runHook preInstall

    install -Dm755 halod $out/bin/halod
    install -Dm755 halod-gui $out/bin/halod-gui
    install -Dm444 udev/60-halod.rules $out/lib/udev/rules.d/60-halod.rules
    install -Dm444 ThirdPartyLicenses/Plugins/licenses.txt \
      $out/share/licenses/halod/plugins/licenses.txt
    install -Dm444 ${assets}/dev.timp4w.Halod.desktop \
      $out/share/applications/dev.timp4w.Halod.desktop
    install -Dm444 ${assets}/icon.svg \
      $out/share/icons/hicolor/scalable/apps/halod.svg

    runHook postInstall
  '';

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
    wrapProgram $out/bin/halod \
      --prefix PATH : ${lib.makeBinPath [ pkgs.ffmpeg ]}
  '';

  meta = {
    description = "Peripheral control daemon (fan curves, RGB, LCD, audio EQ, DPI)";
    homepage = "https://github.com/TimP4w/HaloDaemon";
    license = lib.licenses.gpl3Plus;
    platforms = [ "x86_64-linux" ];
    mainProgram = "halod-gui";
  };
}
