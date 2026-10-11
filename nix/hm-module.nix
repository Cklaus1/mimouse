self: {
  config,
  pkgs,
  lib,
  ...
}:
with lib; let
  cfg = config.programs.mimouse;
  defaultPackage = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
  tomlFormat = pkgs.formats.toml {};
in {
  options.programs.mimouse = with types; {
    enable = mkEnableOption "Whether or not to enable mimouse.";
    package = mkOption {
      type = with types; nullOr package;
      default = defaultPackage;
      defaultText = literalExpression "inputs.mimouse.packages.${pkgs.stdenv.hostPlatform.system}.default";
      description = ''
        The mimouse package to use.

        By default, this option will use the `packages.default` as exposed by this flake.
      '';
    };
    systemd = mkOption {
      type = types.bool;
      default = pkgs.stdenv.isLinux;
      description = "Whether to enable to systemd service for mimouse on linux.";
    };
    launchd = mkOption {
      type = types.bool;
      default = pkgs.stdenv.isDarwin;
      description = "Whether to enable to launchd service for mimouse on macOS.";
    };
    settings = lib.mkOption {
      inherit (tomlFormat) type;
      default = {};
      example = builtins.fromTOML (builtins.readFile (self + /config.toml));
      description = ''
        Optional configuration written to {file}`$XDG_CONFIG_HOME/mimouse/config.toml`.

        See <https://github.com/Cklaus1/mimouse/> for
        available options and documentation.
      '';
    };
  };

  config = mkIf cfg.enable {
    systemd.user.services.mimouse = lib.mkIf cfg.systemd {
      Unit = {
        Description = "Systemd service for MiMouse";
        Requires = ["graphical-session.target"];
      };
      Service = {
        Type = "simple";
        ExecStart = "${cfg.package}/bin/mimouse daemon";
      };
      Install.WantedBy = [
        (lib.mkIf config.wayland.windowManager.hyprland.systemd.enable "hyprland-session.target")
        (lib.mkIf config.wayland.windowManager.sway.systemd.enable "sway-session.target")
      ];
    };

    launchd.agents.mimouse = lib.mkIf cfg.launchd {
      enable = true;
      config = {
        ProgramArguments = [
          "${cfg.package}/bin/mimouse"
          "daemon"
        ];
        KeepAlive = true;
      };
    };

    home.packages = [
      cfg.package
    ];

    xdg.configFile."mimouse/config.toml" = lib.mkIf (cfg.settings != {}) {
      source = tomlFormat.generate "config.toml" cfg.settings;
    };
  };
}
