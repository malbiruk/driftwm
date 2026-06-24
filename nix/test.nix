{ config
, lib
, pkgs
, driftwm-module
, driftwm-package
, ...
}:

pkgs.testers.runNixOSTest {
  name = "driftwm-test";

  nodes.machine = { config, pkgs, ... }: {
    imports = [ driftwm-module ];

    programs.driftwm = {
      enable = true;
      package = driftwm-package;
    };

    # Set up a test user
    users.users.alice = {
      isNormalUser = true;
      uid = 1000;
      extraGroups = [ "wheel" "video" "input" ];
      initialPassword = "alice";
    };

    # Autologin on tty1
    services.getty.autologinUser = "alice";

    # Ensure seatd is running for libseat/udev backend
    services.seatd.enable = true;

    # Need virtual GPU support in QEMU
    virtualisation.qemu.options = [ "-vga none -device virtio-gpu-pci" ];
  };

  testScript = ''
    start_all()
    # Wait for the system to boot to multi-user target
    machine.wait_for_unit("multi-user.target")

    # Verify that the driftwm binary is present and runnable
    print(machine.succeed("driftwm --version"))

    # Verify that the systemd user service can be found/loaded
    machine.succeed("su - alice -c 'systemctl --user list-unit-files | grep driftwm'")
  '';
}
