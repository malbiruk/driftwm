{
  config,
  pkgs,
  lib,
  ...
}:

let
  cfg = config.wayland.windowManager.driftwm;
  tomlFormat = pkgs.formats.toml { };
in

{
  meta.maintainers = [ ];

  options.wayland.windowManager.driftwm = {
    enable = lib.mkEnableOption "driftwm, a trackpad-first infinite canvas Wayland compositor";

    package = lib.mkPackageOption pkgs "driftwm" {
      nullable = true;
      extraDescription = ''
        Set to `null` to not add any driftwm package to your path.
      '';
    };

    systemd = {
      enable = lib.mkEnableOption "systemd" // {
        default = true;
        description = ''
          Whether to install driftwm's systemd units from the {option}`package`,
          that are used by {command}`driftwm-session`.
        '';
      };
    };

    xwaylandSatellitePackage = lib.mkPackageOption pkgs "xwayland-satellite" {
      nullable = true;
      extraDescription = ''
        With `xwayland-satellite` in the {env}`$PATH`, driftwm starts XWayland
        at startup. Set to `null` to skip installing it; to disable XWayland in
        driftwm, set `wayland.windowManager.driftwm.settings.xwayland.enabled = false`.
        See <https://malbiruk.github.io/driftwm/config.html#xwayland>.
      '';
    };

    extraPortals = lib.mkOption {
      type = lib.types.listOf lib.types.package;
      default = [
        pkgs.xdg-desktop-portal-gtk
        pkgs.xdg-desktop-portal-wlr
      ];
      defaultText = lib.literalExpression "[ pkgs.xdg-desktop-portal-gtk pkgs.xdg-desktop-portal-wlr ]";
      description = ''
        Extra portal implementations to use with driftwm.
        driftwm ships a portal configuration ({file}`driftwm-portals.conf`) that
        uses {pkg}`xdg-desktop-portal-gtk` as default and {pkg}`xdg-desktop-portal-wlr`
        for screencasting and screenshots. Set to `[ ]` to not install any portal packages.
      '';
    };

    checkConfig = lib.mkOption {
      type = lib.types.bool;
      default = cfg.package != null;
      defaultText = lib.literalExpression "wayland.windowManager.driftwm.package != null";
      description = "If enabled and package is not null, validates the generated config file.";
    };

    settings = lib.mkOption {
      type = tomlFormat.type;
      default = { };
      description = ''
        Configuration added to {file}`$XDG_CONFIG_HOME/driftwm/config.toml`.
        See <https://github.com/malbiruk/driftwm/blob/main/config.reference.toml>
        for the full list of options.
      '';
      example = lib.literalExpression ''
        {
          mod_key = "super";
          focus_follows_mouse = true;
          window_placement = "center";
          snap = {
            gap = 8;
            outer_gap = 8;
          };
          keybindings = {
            "mod+Return" = "spawn ghostty";
          };
        }
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      (lib.hm.assertions.assertPlatform "wayland.windowManager.driftwm" pkgs lib.platforms.linux)
      {
        assertion = cfg.systemd.enable -> cfg.package != null;
        message = "wayland.windowManager.driftwm.systemd.enable requires a non-null package";
      }
      {
        assertion = cfg.checkConfig -> cfg.package != null;
        message = "wayland.windowManager.driftwm.checkConfig requires a non-null package";
      }
    ];

    home.packages = lib.concatLists [
      (lib.optional (cfg.package != null) cfg.package)
      (lib.optional (cfg.xwaylandSatellitePackage != null) cfg.xwaylandSatellitePackage)
    ];

    systemd.user.packages = lib.optional cfg.systemd.enable cfg.package;

    # Keyring for managing secrets, declared in driftwm-portals.conf
    services.gnome-keyring.enable = lib.mkDefault true;

    # XDG Desktop Portals integration
    xdg.portal = lib.mkIf (cfg.extraPortals != [ ]) {
      enable = true;
      extraPortals = cfg.extraPortals;
      configPackages = lib.optional (cfg.package != null) cfg.package;
    };

    xdg.configFile."driftwm/config.toml" = lib.mkIf (cfg.settings != { }) {
      source =
        let
          configFile = tomlFormat.generate "driftwm-config.toml" cfg.settings;
        in
        pkgs.runCommand "driftwm-config.toml"
          {
            nativeBuildInputs = lib.optional cfg.checkConfig cfg.package;
          }
          ''
            cp ${configFile} $out
            ${lib.optionalString cfg.checkConfig ''
              ${lib.getExe cfg.package} --check-config --config "$out"
            ''}
          '';
    };
  };
}
