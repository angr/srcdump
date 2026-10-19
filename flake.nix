{
  description = "srcdump: an objdump-like tool that prints decompiled code, backed by angr";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
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
        cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);

        # srcdump itself. At runtime it provisions angr with uv, so put
        # nixpkgs' uv on PATH instead of letting srcdump download one.
        srcdump = pkgs.rustPlatform.buildRustPackage {
          pname = "srcdump";
          inherit (cargoToml.package) version;

          src = pkgs.lib.cleanSource ./.;
          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = [ pkgs.makeWrapper ];

          postInstall = ''
            wrapProgram $out/bin/srcdump \
              --suffix PATH : ${pkgs.lib.makeBinPath [ pkgs.uv ]}
          '';

          meta = {
            inherit (cargoToml.package) description homepage;
            license = pkgs.lib.licenses.bsd2;
            mainProgram = "srcdump";
          };
        };
      in
      {
        packages = {
          inherit srcdump;
          default = srcdump;
        };

        apps.default = flake-utils.lib.mkApp { drv = srcdump; };

        devShells.default = pkgs.mkShell {
          inputsFrom = [ srcdump ];
          packages = with pkgs; [
            cargo
            rustc
            rustfmt
            clippy
            rust-analyzer
            uv
          ];
        };
      }
    );
}
