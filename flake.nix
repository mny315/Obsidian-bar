{
  description = "Obsidian Bar GTK4 layer-shell bar";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (pkgs:
        let
          cargoPackage = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package;
          package = pkgs.rustPlatform.buildRustPackage {
            pname = cargoPackage.name;
            version = cargoPackage.version;
            src = ./.;

            cargoLock.lockFile = ./Cargo.lock;

            nativeBuildInputs = [ pkgs.pkg-config pkgs.rustPlatform.bindgenHook ];
            buildInputs = [ pkgs.gdk-pixbuf pkgs.gtk4 pkgs.gtk4-layer-shell pkgs.pipewire ];
            OBSIDIAN_BAR_MPVPAPER_BIN = "${pkgs.mpvpaper}/bin/mpvpaper";
            OBSIDIAN_BAR_SWAYBG_BIN = "${pkgs.swaybg}/bin/swaybg";
            OBSIDIAN_BAR_FFMPEG_BIN = "${pkgs.ffmpeg}/bin/ffmpeg";
            OBSIDIAN_BAR_BRIGHTNESSCTL_BIN = "${pkgs.brightnessctl}/bin/brightnessctl";
            OBSIDIAN_BAR_DDCUTIL_BIN = "${pkgs.ddcutil}/bin/ddcutil";
            OBSIDIAN_BAR_PW_DUMP_BIN = "${pkgs.pipewire}/bin/pw-dump";
            OBSIDIAN_BAR_WPCTL_BIN = "${pkgs.wireplumber}/bin/wpctl";
            OBSIDIAN_BAR_KILL_BIN = "${pkgs.coreutils}/bin/kill";

            meta.mainProgram = "obsidian-bar";
          };
        in {
          default = package;
          obsidian-bar = package;
        });

      apps = forAllSystems (pkgs: {
        default = {
          type = "app";
          program = "${self.packages.${pkgs.stdenv.hostPlatform.system}.default}/bin/obsidian-bar";
        };
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.stdenv.hostPlatform.system}.default ];
          packages = with pkgs; [
            cargo
            rustc
            rustfmt
            clippy
            mpvpaper
            swaybg
            ffmpeg
            brightnessctl
            ddcutil
            pipewire
            wireplumber
          ];
        };
      });
    };
}
