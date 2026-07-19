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
    path = src + "/extensions/companion@halod.timp4w.dev";
    name = "gnome-shell-extension-halod-src";
  };
  installPhase = ''
    install -Dm644 extension.js \
      $out/share/gnome-shell/extensions/companion@halod.timp4w.dev/extension.js
    install -Dm644 metadata.json \
      $out/share/gnome-shell/extensions/companion@halod.timp4w.dev/metadata.json
  '';
}
