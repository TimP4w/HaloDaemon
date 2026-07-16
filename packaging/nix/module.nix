# NixOS integration. In your configuration:
#   imports = [ halod.nixosModules.default ];
#   services.halod.enable = true;
{
  self,
  mkHalodExtension,
}:
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

    autostart = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = ''
        Launch HaloDaemon at login: a system XDG autostart entry starts
        the GUI in background (tray) mode, and a systemd user service
        runs the daemon for the graphical session. Set to `false` to
        keep it from starting on boot — start the GUI manually instead,
        which brings the daemon up on demand.

        This is the declarative equivalent of the in-app "Start on boot"
        toggle, which cannot work on NixOS: it manages a per-user file
        under `~/.config/autostart`, but the store paths shipped here are
        read-only, so the toggle can neither observe nor remove them.
      '';
    };

    i2c = {
      enable = lib.mkEnableOption ''
        SMBus / I2C access for chipset RGB (ASUS/ENE DRAM and GPU). Turns
        on `hardware.i2c.enable`, which loads i2c-dev. Generated plugin
        rules grant matching adapters to the `halod` group'';

      platform = lib.mkOption {
        type = lib.types.nullOr (
          lib.types.enum [
            "intel"
            "amd"
          ]
        );
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
    ]
    ++ lib.optional cfg.enableGnomeExtension (mkHalodExtension pkgs);

    # hidraw / i2c-dev rules derived from embedded plugins, plus the daemon's
    # uinput/hwmon baseline rules.
    services.udev.packages = [ cfg.package ];

    # hwmon PWM control files are scoped to this group (mode 0664) rather
    # than made world-writable. Add your user to drive fans without root:
    #   users.users.<you>.extraGroups = [ "halod" ];
    users.groups.halod = { };

    # SMBus (DRAM + GPU RGB). hardware.i2c.enable loads i2c-dev; generated
    # plugin rules grant matching adapters to the halod group above.
    hardware.i2c.enable = lib.mkIf cfg.i2c.enable true;

    # Chip-specific kernel modules, opt-in per board. i2c-dev itself is
    # already handled by hardware.i2c.enable above.
    boot.kernelModules =
      lib.optional cfg.enableNuvotonFanControl "nct6775"
      ++ lib.optionals (cfg.i2c.enable && cfg.i2c.platform == "amd") [ "i2c-piix4" ]
      ++ lib.optionals (cfg.i2c.enable && cfg.i2c.platform == "intel") [ "i2c-i801" ];

    # Autostart the UI at login in background mode (no window shown).
    # NoDisplay=true keeps this out of the app grid — only the
    # share/applications entry is visible to the user. Gated on
    # `autostart` so users can opt out of starting on boot.
    environment.etc."xdg/autostart/halod.desktop" = lib.mkIf cfg.autostart {
      text = ''
        [Desktop Entry]
        Name=HaloDaemon
        Exec=${cfg.package}/bin/halod-gui --background
        Icon=halod
        Terminal=false
        Type=Application
        NoDisplay=true
      '';
    };

    # The IPC socket lives in $XDG_RUNTIME_DIR, so the daemon runs as a
    # per-user service tied to the graphical session.
    systemd.user.services.halod = lib.mkIf cfg.autostart {
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
}
