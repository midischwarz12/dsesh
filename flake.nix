{
  description = "dsesh: a small detachable terminal session runner with retained screen state";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forEachSystem = nixpkgs.lib.genAttrs systems;
    in
    {
      packages = forEachSystem (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          dsesh = pkgs.rustPlatform.buildRustPackage {
            pname = "dsesh";
            version = "0.1.0";
            src = self;
            cargoLock.lockFile = ./Cargo.lock;
            nativeBuildInputs = [ pkgs.installShellFiles ];
            postInstall = ''
              installManPage doc/dsesh.1
            '';
          };
        in
        {
          default = dsesh;
          dsesh = dsesh;
        });

      checks = forEachSystem (system: {
        default = self.packages.${system}.default;
      });

      apps = forEachSystem (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.dsesh}/bin/dsesh";
        };
      });

      devShells = forEachSystem (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
            packages = [
              pkgs.cargo
              pkgs.clippy
              pkgs.rustc
              pkgs.rustfmt
              pkgs.rust-analyzer
              pkgs.pkg-config
            ];

            RUST_BACKTRACE = "1";
          };
        });
    };
}
