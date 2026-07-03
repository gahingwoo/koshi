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
            gtk4
            libadwaita
          ];

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
            gtk4
            libadwaita
          ];

          # `cargo run` bypasses wrapGAppsHook4, so expose GSettings schemas
          # manually to avoid runtime aborts about missing schemas.
          shellHook = ''
            export XDG_DATA_DIRS=${pkgs.gsettings-desktop-schemas}/share/gsettings-schemas/${pkgs.gsettings-desktop-schemas.name}:${pkgs.gtk4}/share/gsettings-schemas/${pkgs.gtk4.name}:$XDG_DATA_DIRS
          '';
        };
      });
    };
}
