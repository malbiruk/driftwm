{ config
, lib
, pkgs
, ... 
}:

let
  cfg = config.programs.driftwm;
in

{
  options.programs.driftwm = {
    enable = lib.mkEnableOption "driftwm, a trackpad-first infinite canvas Wayland compositor";

    package = lib.mkPackageOption pkgs "driftwm" { };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];

    services.displayManager.sessionPackages = [ cfg.package ];

    # Required for Wayland sessions
    hardware.graphics.enable = lib.mkDefault true;
    security.polkit.enable = lib.mkDefault true;

    # Screen lockers like swaylock need PAM configuration to authenticate
    security.pam.services.swaylock = lib.mkDefault { };

    # Expose systemd user units provided by driftwm package
    systemd.packages = [ cfg.package ];

    # Stop driftwm service from clobbering the imported session PATH
    systemd.user.services.driftwm = {
      restartIfChanged = false;
      enableDefaultPath = false;
    };

    # XDG Desktop Portals integration
    xdg.portal = {
      enable = lib.mkDefault true;
      configPackages = lib.mkDefault [ cfg.package ];
      extraPortals = lib.mkDefault [
        pkgs.xdg-desktop-portal-gtk
        pkgs.xdg-desktop-portal-wlr
      ];
    };
  };
}
