{
  description = "cargo-pulse: abandonment risk for crates.io packages";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
      manifest = (nixpkgs.lib.importTOML ./Cargo.toml).package;
    in
    {
      packages = forAll (pkgs: rec {
        cargo-pulse = pkgs.rustPlatform.buildRustPackage {
          pname = manifest.name;
          version = manifest.version;
          # model artifacts in data/ are include_bytes!'d, so they ship in src
          src = pkgs.lib.cleanSource ./.;
          cargoLock.lockFile = ./Cargo.lock;

          # Nix builds run without network access, so the live lookups can't be
          # tested here. --parity is fully offline (model + reference sample are
          # compiled in) and checks the built binary scores like the Python model.
          doCheck = true;
          checkPhase = ''
            runHook preCheck
            "$releaseDir"/cargo-pulse --parity
            runHook postCheck
          '';

          meta = {
            inherit (manifest) description;
            homepage = manifest.repository;
            license = pkgs.lib.licenses.agpl3Plus;
            mainProgram = "cargo-pulse";
          };
        };
        default = cargo-pulse;
      });

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [ cargo rustc rustfmt clippy rust-analyzer ];
        };
      });
    };
}
