{
  description = "driftwm — a trackpad-first infinite canvas Wayland compositor";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      pkgsFor = system: nixpkgs.legacyPackages.${system};

      nativeBuildInputsFor = pkgs: with pkgs; [
        pkg-config
      ];

      buildInputsFor = pkgs: with pkgs; [
        wayland
        wayland-protocols
        seatd # libseat
        libdisplay-info
        libinput
        libgbm
        libxkbcommon
        libdrm
        systemd # libudev
        libglvnd
        libx11
        libxcursor
        libxrandr
        libxi
        libxcb
        pixman
      ];

      runtimeLibsFor = pkgs: with pkgs; [
        wayland
        seatd
        libdisplay-info
        libinput
        libgbm
        libxkbcommon
        libdrm
        systemd
        libglvnd
        libx11
        libxcursor
        libxrandr
        libxi
        libxcb
        pixman
      ];
    in
    {
      packages = forAllSystems (system:
        let pkgs = pkgsFor system;
        in {
          default = pkgs.rustPlatform.buildRustPackage rec {
            pname = "driftwm";
            version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;

            src = pkgs.lib.cleanSourceWith {
              src = ./.;
              filter = path: type:
                let baseName = builtins.baseNameOf path;
                in baseName != "target" && baseName != ".git" && baseName != ".direnv";
            };

            cargoLock = {
              lockFile = ./Cargo.lock;
              allowBuiltinFetchGit = true;
            };

            # Running the suite is CI's job — don't make every user's rebuild pay for it.
            doCheck = false;

            nativeBuildInputs = nativeBuildInputsFor pkgs;
            buildInputs = buildInputsFor pkgs;

            # Make sure the binary can find shared libraries at runtime
            postFixup = ''
              patchelf --add-rpath "${pkgs.lib.makeLibraryPath (runtimeLibsFor pkgs)}" $out/bin/driftwm
            '';

            postInstall = ''
              install -Dm755 resources/driftwm-session $out/bin/driftwm-session
              install -Dm644 resources/driftwm.desktop $out/share/wayland-sessions/driftwm.desktop
              install -Dm644 resources/driftwm-portals.conf $out/share/xdg-desktop-portal/driftwm-portals.conf
              install -Dm644 resources/driftwm.service $out/lib/systemd/user/driftwm.service
              install -Dm644 resources/driftwm-shutdown.target $out/lib/systemd/user/driftwm-shutdown.target
              install -Dm644 config.reference.toml $out/etc/driftwm/config.reference.toml
              for f in extras/wallpapers/*.glsl; do
                install -Dm644 "$f" "$out/share/driftwm/wallpapers/$(basename "$f")"
              done

            substituteInPlace $out/share/wayland-sessions/driftwm.desktop --replace-fail "Exec=driftwm-session" "Exec=$out/bin/driftwm-session"

            substituteInPlace $out/lib/systemd/user/driftwm.service --replace-fail "ExecStart=driftwm" "ExecStart=$out/bin/driftwm"
            '';

            passthru.providedSessions = [ "driftwm" ];

            meta = with pkgs.lib; {
              description = "A trackpad-first infinite canvas Wayland compositor";
              license = licenses.gpl3Plus;
              platforms = systems;
              mainProgram = "driftwm";
            };
          };
        });

      devShells = forAllSystems (system:
        let pkgs = pkgsFor system;
        in {
          default = pkgs.mkShell {
            inputsFrom = [ self.packages.${system}.default ];
            packages = [ pkgs.rustfmt ];

            LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath (runtimeLibsFor pkgs);
          };
        });

      checks = forAllSystems (system:
        let pkgs = pkgsFor system;
        in {
          default = pkgs.callPackage ./nix/test.nix {
            driftwm-module = self.nixosModules.default;
            driftwm-package = self.packages.${system}.default;
          };
        });

      nixosModules.driftwm = { config, lib, pkgs, ... }: {
        imports = [ ./nix/nixos-module.nix ];
        programs.driftwm.package = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      };

      nixosModules.default = self.nixosModules.driftwm;
    };
}
