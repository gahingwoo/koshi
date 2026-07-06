{
  description = "Koshi - a GTK4 + libadwaita application";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems
        (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (pkgs: {
        default = pkgs.rustPlatform.buildRustPackage {
          pname = "koshi";
          version = "0.1.0";
          src = self;

          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = with pkgs; [
            pkg-config
            wrapGAppsHook4
          ];

          buildInputs = with pkgs; [
            glib
            glib-networking
            gtk4
            libadwaita
            libsoup_3
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
            description = "Koshi";
            mainProgram = "koshi";
            platforms = pkgs.lib.platforms.linux;
          };
        };
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [
            rustc
            cargo
            rust-analyzer
            rustfmt
            clippy
            pkg-config
          ];

          buildInputs = with pkgs; [
            glib
            glib-networking
            gtk4
            libadwaita
            libsoup_3
          ];

          # `cargo run` bypasses wrapGAppsHook4, so expose GSettings schemas
          # manually to avoid runtime aborts about missing schemas, and the
          # glib-networking GIO modules so TLS works.
          shellHook = ''
            export XDG_DATA_DIRS=${pkgs.gsettings-desktop-schemas}/share/gsettings-schemas/${pkgs.gsettings-desktop-schemas.name}:${pkgs.gtk4}/share/gsettings-schemas/${pkgs.gtk4.name}:$XDG_DATA_DIRS
            export GIO_EXTRA_MODULES=${pkgs.glib-networking}/lib/gio/modules''${GIO_EXTRA_MODULES:+:$GIO_EXTRA_MODULES}
          '';
        };
      });
    };
}
