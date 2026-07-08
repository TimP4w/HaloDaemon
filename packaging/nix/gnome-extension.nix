# GNOME Shell extension — copies the focus-watcher extension into the Nix
# store so NixOS can pick it up from share/gnome-shell/extensions/.
{
  pkgs,
  version,
  src,
}:
pkgs.stdenv.mkDerivation {
  pname = "gnome-shell-extension-halod";
  inherit version;
  src = builtins.path {
    path = src + "/extensions/halod@halod";
    name = "gnome-shell-extension-halod-src";
  };
  installPhase = ''
    install -Dm644 extension.js \
      $out/share/gnome-shell/extensions/halod@halod/extension.js
    install -Dm644 metadata.json \
      $out/share/gnome-shell/extensions/halod@halod/metadata.json
  '';
}
