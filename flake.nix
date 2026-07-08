{
  description = "HaloDaemon — peripheral control daemon with an egui UI";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      lib = nixpkgs.lib;
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forEachSystem = lib.genAttrs systems;

      # The release version is owned by the Cargo workspace (src/Cargo.toml).
      version = (lib.importTOML ./src/Cargo.toml).workspace.package.version;

      # Release / packaging derivations live under packaging/nix/; only the
      # dev shell is kept inline below.
      mkHalodExtension =
        pkgs:
        import ./packaging/nix/gnome-extension.nix {
          inherit pkgs version;
          src = ./.;
        };

      mkHalod =
        pkgs:
        import ./packaging/nix/package.nix {
          inherit pkgs lib version;
          buildHash = self.shortRev or self.dirtyShortRev or "unknown";
          src = ./.;
        };
    in
    {
      packages = forEachSystem (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        rec {
          halod = mkHalod pkgs;
          gnome-extension = mkHalodExtension pkgs;
          default = halod;
        }
      );

      nixosModules.default = import ./packaging/nix/module.nix {
        inherit self mkHalodExtension;
      };

      devShells = forEachSystem (
        system:
        let
          pkgs = import nixpkgs {
            inherit system;
          };
        in
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              clippy
              rust-analyzer
              rustc
              rustfmt
              reuse
              cargo-about

              gcc
              pkg-config
              clang
              llvmPackages.libclang

              hidapi
              i2c-tools
              libusb1
              pciutils
              udev
              usbutils

              libxkbcommon
              pipewire
              wayland

              # Runtime tool: the LCD video engine spawns `ffmpeg` from PATH.
              ffmpeg
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

            env = {
              RUST_BACKTRACE = "1";
              RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
              LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
              # eframe/winit dlopens these at runtime.
              LD_LIBRARY_PATH = lib.makeLibraryPath [
                pkgs.hidapi
                pkgs.libusb1
                pkgs.pipewire
                pkgs.udev
                pkgs.wayland
                pkgs.libxkbcommon
                pkgs.libGL
              ];
            };

            shellHook = ''
              echo "HaloDaemon dev shell ready."
            '';
          };
        }
      );
    };
}
