{
  description = "Shiny Portal | @PZeide";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = {
    nixpkgs,
    flake-utils,
    ...
  }: (flake-utils.lib.eachSystem ["x86_64-linux" "aarch64-linux"] (
    system: let
      pkgs = nixpkgs.legacyPackages.${system};
      xdg-desktop-portal-shiny = pkgs.callPackage ./default.nix {};
    in {
      packages = rec {
        inherit xdg-desktop-portal-shiny;
        default = xdg-desktop-portal-shiny;
      };
    }
  ));
}
