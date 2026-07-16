{
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
      in
      {
        formatter = pkgs.nixfmt-rfc-style;

        packages.default =
          let
            pname = "koshi";
            version = "0.1.0";
            src = self;
          in
          pkgs.rustPlatform.buildRustPackage {
            inherit pname version src;

            cargoLock.lockFile = ./Cargo.lock;

            # The send tests create scratch files under glib's user runtime
            # dir; in the build sandbox XDG_RUNTIME_DIR is unset and glib's
            # fallback (under HOME) is unwritable.
            preCheck = ''
              export XDG_RUNTIME_DIR=$(mktemp -d)
            '';

            nativeBuildInputs = [
              pkgs.pkg-config
              pkgs.wrapGAppsHook4
            ];

            buildInputs = [
              pkgs.glib
              pkgs.glib-networking
              pkgs.gtk4
              pkgs.libadwaita
              pkgs.libsoup_3
            ];

            # Install the desktop entry and hicolor app icons so the shell and
            # launchers can find Koshi's icon by its application ID.
            postInstall = ''
              install -Dm644 data/moe.nikableh.Koshi.desktop \
                $out/share/applications/moe.nikableh.Koshi.desktop
              install -Dm644 data/icons/scalable/apps/moe.nikableh.Koshi.svg \
                $out/share/icons/hicolor/scalable/apps/moe.nikableh.Koshi.svg
              install -Dm644 data/icons/scalable/apps/moe.nikableh.Koshi.Devel.svg \
                $out/share/icons/hicolor/scalable/apps/moe.nikableh.Koshi.Devel.svg
              install -Dm644 data/icons/symbolic/apps/moe.nikableh.Koshi-symbolic.svg \
                $out/share/icons/hicolor/symbolic/apps/moe.nikableh.Koshi-symbolic.svg
            '';

            meta = {
              description = "Read and reply to kernel mailing lists";
              homepage = "https://github.com/nikableh/koshi";
              license = pkgs.lib.licenses.gpl3Only;
              mainProgram = "koshi";
              platforms = pkgs.lib.platforms.linux;
            };
          };

        apps.default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/koshi";
        };

        devShells.default = pkgs.mkShell {
          inputsFrom = [ self.packages.${system}.default ];
          buildInputs = [
            pkgs.bashInteractive
            pkgs.rust-analyzer
            pkgs.rustfmt
            pkgs.clippy
            # Enable services.flatpak.enable = true; in configuration.nix, it
            # won't work without it.
            pkgs.flatpak
            pkgs.flatpak-builder
          ];

          # `cargo run` bypasses wrapGAppsHook4, so expose GSettings schemas
          # manually to avoid runtime aborts about missing schemas, and the
          # glib-networking GIO modules so TLS works.
          shellHook = ''
            export XDG_DATA_DIRS=${pkgs.gsettings-desktop-schemas}/share/gsettings-schemas/${pkgs.gsettings-desktop-schemas.name}:${pkgs.gtk4}/share/gsettings-schemas/${pkgs.gtk4.name}:$XDG_DATA_DIRS
            export GIO_EXTRA_MODULES=${pkgs.glib-networking}/lib/gio/modules''${GIO_EXTRA_MODULES:+:$GIO_EXTRA_MODULES}
          '';
        };
      }
    );
}
