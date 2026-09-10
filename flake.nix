{
  description = "sqlx-query — splices SQL fragments into the sentinel comments of a query, for sqlx";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    devcontainer-env.url = "github:devcontainer-env/devcontainer-env";
  };

  outputs =
    {
      nixpkgs,
      flake-utils,
      rust-overlay,
      devcontainer-env,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };
        manifest = (pkgs.lib.importTOML ./Cargo.toml).package;
        rust-toolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
      in
      {
        # No `packages.default`. This is a library crate with no binary, and
        # `buildRustPackage` would want a committed Cargo.lock -- which a library
        # deliberately does not have. The dev shell is the whole point here.
        devShells.default = pkgs.mkShell {
          inherit (manifest) name;

          packages = with pkgs; [
            rust-toolchain
            pkg-config
            # A CLI only, for poking at what the tests leave behind. The
            # Postgres tests/postgres.rs runs against is the compose service in
            # .devcontainer, not a server started here.
            postgresql
            devcontainer-env.packages.${system}.default
          ];

          # DATABASE_URL is defined once, in .devcontainer/devcontainer.json.
          # `export` reads it from `containerEnv` and rewrites the compose
          # hostname to the port Docker assigned, so the same definition is
          # correct inside the container, on the host, and on a CI runner.
          #
          # Tolerating failure is deliberate: with no devcontainer running --
          # a contributor without Docker, or someone only touching the unit
          # tests -- DATABASE_URL stays unset and tests/postgres.rs skips,
          # which is its documented behaviour.
          shellHook = ''
            eval "$(devcontainer-env export 2>/dev/null)" 2>/dev/null || true

            # Only greet a human. CI drives this shell with
            # `nix develop --command ...`, where a banner is just noise in front
            # of the output someone is actually reading.
            if [[ $- == *i* ]]; then
              echo "${manifest.name} ${manifest.version} — $(cargo --version)"
              if [[ -n "''${DATABASE_URL:-}" ]]; then
                echo "  cargo test          # incl. the end-to-end Postgres round trip"
              else
                echo "  cargo test          # Postgres tests skip; start the devcontainer for them"
              fi
              echo "  cargo clippy --all-targets"
            fi
          '';
        };
      }
    );
}
