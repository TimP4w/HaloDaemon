{
  description = "HaloDaemon — peripheral control daemon with a GTK4 UI";

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

      # GNOME Shell extension — copies the focus-watcher extension into the Nix
      # store so NixOS can pick it up from share/gnome-shell/extensions/.
      mkHalodExtension =
        pkgs:
        pkgs.stdenv.mkDerivation {
          pname = "gnome-shell-extension-halod";
          inherit version;
          src = builtins.path {
            path = ./. + "/extensions/halod@halod";
            name = "gnome-shell-extension-halod-src";
          };
          installPhase = ''
            install -Dm644 extension.js \
              $out/share/gnome-shell/extensions/halod@halod/extension.js
            install -Dm644 metadata.json \
              $out/share/gnome-shell/extensions/halod@halod/metadata.json
          '';
        };

      # The HaloDaemon package — builds halod and halod-gui from the cargo workspace.
      mkHalod =
        pkgs:
        pkgs.rustPlatform.buildRustPackage {
          pname = "halod";
          inherit version;

          # Only what cargo needs — keeps build artifacts and .git out of the
          # Nix store and off every evaluation.
          src = lib.cleanSourceWith {
            src = ./.;
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

          cargoLock = {
            lockFile = ./src/Cargo.lock;
            outputHashes = {
              "q565-0.4.0" = "sha256-3DHMdzWa1e+51VGLp5Q/bFsvNfNssrDG9RteWm5Z7lA=";
            };
          };

          nativeBuildInputs = with pkgs; [
            pkg-config
            wrapGAppsHook4
            rustPlatform.bindgenHook
            cargo-about
          ];

          buildInputs = with pkgs; [
            gtk4
            libadwaita
            hidapi
            libusb1
            pipewire
            libpulseaudio
            udev
            wayland
            dbus
            libxkbcommon
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

          meta = {
            description = "Peripheral control daemon with a GTK4 UI (fan curves, RGB, LCD, audio EQ, DPI)";
            homepage = "https://github.com/TimP4w/HaloDaemon";
            license = lib.licenses.gpl3Plus;
            platforms = lib.platforms.linux;
            mainProgram = "halod-gui";
          };
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

      # NixOS integration. In your configuration:
      #   imports = [ halod.nixosModules.default ];
      #   services.halod.enable = true;
      nixosModules.default =
        {
          config,
          lib,
          pkgs,
          ...
        }:
        let
          cfg = config.programs.halod;
        in
        {
          options.programs.halod = {
            enable = lib.mkEnableOption "the HaloDaemon peripheral control daemon";
            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${pkgs.system}.default;
              defaultText = lib.literalExpression "halod.packages.\${system}.default";
              description = "The HaloDaemon package to use.";
            };

            enableGnomeExtension = lib.mkEnableOption ''
              the HaloDaemon GNOME Shell focus-watcher extension (foreground-app
              detection on GNOME Wayland). Installs it system-wide; each user
              still enables it with `gnome-extensions enable halod@halod`'';

            i2c = {
              enable = lib.mkEnableOption ''
                SMBus / I2C access for chipset RGB (ASUS/ENE DRAM and GPU). Turns
                on `hardware.i2c.enable`, which loads i2c-dev and creates the
                `i2c` group — add your user to it with
                `users.users.<you>.extraGroups = [ "i2c" ]`'';

              platform = lib.mkOption {
                type = lib.types.nullOr (lib.types.enum [
                  "intel"
                  "amd"
                ]);
                default = null;
                example = "amd";
                description = ''
                  Chipset SMBus controller driver to load:
                  "intel" → i2c-i801, "amd" → i2c-piix4. `null` loads neither
                  (use it if the driver autoloads on your board).
                '';
              };

            };

            enableNuvotonFanControl = lib.mkEnableOption ''
              loading the nct6775 kernel module, which exposes Nuvoton NCT677x
              SuperIO chips (most AMD/Intel consumer boards) as hwmon devices for
              motherboard temperature sensors and PWM fan control. This is SuperIO
              (LPC port I/O), unrelated to I2C. Harmless if the chip is absent'';
          };

          config = lib.mkIf cfg.enable {
            # halod / halod-gui on PATH. The GNOME Shell focus-watcher extension
            # (foreground-app detection on GNOME Wayland) is opt-in: installing it
            # under share/gnome-shell/extensions only makes it discoverable; each
            # user still enables it with `gnome-extensions enable halod@halod`.
            environment.systemPackages = [
              cfg.package
            ] ++ lib.optional cfg.enableGnomeExtension (mkHalodExtension pkgs);

            # hidraw / uinput / i2c-dev access rules shipped by the package.
            services.udev.packages = [ cfg.package ];

            # SMBus (DRAM + GPU RGB). hardware.i2c.enable loads i2c-dev and
            # creates the `i2c` group — add your user with
            #   users.users.<you>.extraGroups = [ "i2c" ];
            hardware.i2c.enable = lib.mkIf cfg.i2c.enable true;

            # Chip-specific kernel modules, opt-in per board. i2c-dev itself is
            # already handled by hardware.i2c.enable above.
            boot.kernelModules =
              lib.optional cfg.enableNuvotonFanControl "nct6775"
              ++ lib.optionals (cfg.i2c.enable && cfg.i2c.platform == "amd") [ "i2c-piix4" ]
              ++ lib.optionals (cfg.i2c.enable && cfg.i2c.platform == "intel") [ "i2c-i801" ];

            # Autostart the UI at login in background mode (no window shown).
            # NoDisplay=true keeps this out of the app grid — only the
            # share/applications entry is visible to the user.
            environment.etc."xdg/autostart/halod.desktop".text = ''
              [Desktop Entry]
              Name=HaloDaemon
              Exec=${cfg.package}/bin/halod-gui --background
              Icon=halod
              Terminal=false
              Type=Application
              NoDisplay=true
            '';

            # The IPC socket lives in $XDG_RUNTIME_DIR, so the daemon runs as a
            # per-user service tied to the graphical session.
            systemd.user.services.halod = {
              description = "HaloDaemon device daemon";
              wantedBy = [ "graphical-session.target" ];
              partOf = [ "graphical-session.target" ];
              after = [ "graphical-session.target" ];
              serviceConfig = {
                ExecStart = "${cfg.package}/bin/halod";
                Restart = "on-failure";
                RestartSec = 2;
              };
            };
          };
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

              dbus
              gtk4
              libadwaita
              hidapi
              i2c-tools
              libusb1
              pciutils
              pulseaudio
              udev
              usbutils

              libxkbcommon
              pipewire
              wayland
              wayland-protocols
              xdg-desktop-portal
            ];

            buildInputs = with pkgs; [
              gtk4
              libadwaita
              hidapi
              libusb1
              pipewire
              pulseaudio
              udev
              wayland
            ];

            env = {
              RUST_BACKTRACE = "1";
              RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
              LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
              LD_LIBRARY_PATH = lib.makeLibraryPath [
                pkgs.gtk4
                pkgs.libadwaita
                pkgs.hidapi
                pkgs.libusb1
                pkgs.pipewire
                pkgs.pulseaudio
                pkgs.udev
                pkgs.wayland
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
