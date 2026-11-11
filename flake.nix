{
  description = "navidrome-proxy: an OpenSubsonic proxy that puts an external catalog inside Navidrome";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      inherit (nixpkgs) lib;
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAll = f: lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      overlays.default = final: _prev: {
        navidrome-proxy = final.callPackage ./nix/package.nix { };
      };

      packages = forAll (pkgs: rec {
        navidrome-proxy = pkgs.callPackage ./nix/package.nix { };
        default = navidrome-proxy;
      });

      nixosModules.default =
        { pkgs, ... }:
        {
          imports = [ ./nix/module.nix ];
          services.navidrome-proxy.package = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        };

      checks = forAll (pkgs: {
        package = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        nixos = pkgs.testers.runNixOSTest (import ./nix/test.nix { module = self.nixosModules.default; });
      });

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            clippy
            rustfmt
            rust-analyzer
            ffmpeg-headless
            chromaprint
            yt-dlp
            sqlite
          ];
        };
      });
    };
}
