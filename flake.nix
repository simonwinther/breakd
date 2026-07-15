{
  description = "Wayland-native break reminder with multi-monitor overlays";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
      packagesFor =
        system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        {
          breakd = pkgs.callPackage ./packaging/nix/package.nix { };
          breakd-relay = pkgs.callPackage ./packaging/nix/relay.nix { };
        };
    in
    {
      packages = forAllSystems (system: {
        inherit (packagesFor system) breakd breakd-relay;
        default = self.packages.${system}.breakd;
      });

      checks = forAllSystems (system: {
        inherit (self.packages.${system}) breakd breakd-relay;
      });

      overlays.default = final: _prev: {
        breakd = final.callPackage ./packaging/nix/package.nix { };
        breakd-relay = final.callPackage ./packaging/nix/relay.nix { };
      };

      nixosModules = {
        breakd = import ./packaging/nix/module.nix { inherit self; };
        default = self.nixosModules.breakd;
      };

      formatter = forAllSystems (system: nixpkgs.legacyPackages.${system}.nixfmt);
    };
}
