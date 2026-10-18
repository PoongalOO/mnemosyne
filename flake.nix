{
  description = "Mnémosyne : lecteur vidéo minimaliste (H.264 / H.265) avec montage simple";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (pkgs: rec {
        mnemosyne = pkgs.callPackage ./nix/package.nix { };
        default = mnemosyne;
      });

      apps = forAllSystems (pkgs: {
        default = {
          type = "app";
          program = "${self.packages.${pkgs.system}.default}/bin/mnemosyne";
        };
      });

      overlays.default = final: prev: {
        mnemosyne = final.callPackage ./nix/package.nix { };
      };

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.system}.default ];
          packages = [ pkgs.cargo pkgs.rustc pkgs.clippy pkgs.rustfmt ];
          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath self.packages.${pkgs.system}.default.runtimeLibs;
        };
      });
    };
}
