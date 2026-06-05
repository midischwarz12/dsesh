# SPDX-FileCopyrightText: 2026 midischwarz12
# SPDX-License-Identifier: AGPL-3.0-or-later

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
          dr = pkgs.writeShellApplication {
            name = "dr";
            runtimeInputs = [ pkgs.coreutils ];
            text = ''
              if [ "$#" -eq 0 ]; then
                echo "usage: dr COMMAND [ARGS...]" >&2
                exit 64
              fi

              mkdir -p /tmp/.dsesh

              if [ -r /proc/sys/kernel/random/uuid ]; then
                IFS= read -r uuid < /proc/sys/kernel/random/uuid
              elif command -v uuidgen >/dev/null 2>&1; then
                uuid="$(uuidgen)"
              else
                echo "dr: could not generate a UUID" >&2
                exit 1
              fi

              exec ${dsesh}/bin/dsesh run "/tmp/.dsesh/$uuid.sock" -- "$@"
            '';
          };
        in
        {
          default = dsesh;
          dsesh = dsesh;
          dr = dr;
        });

      checks = forEachSystem (system: {
        default = self.packages.${system}.default;
        dr = self.packages.${system}.dr;
      });

      apps = forEachSystem (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.dsesh}/bin/dsesh";
        };
        dr = {
          type = "app";
          program = "${self.packages.${system}.dr}/bin/dr";
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
