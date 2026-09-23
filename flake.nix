{
  description = "sqlx-query - splices where/order-by/cursor clauses into the sentinel comments of a query you already wrote.";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    devcontainer-env.url = "github:devcontainer-env/devcontainer-env";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      nixpkgs,
      flake-utils,
      devcontainer-env,
      rust-overlay,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };

        # From rust-toolchain.toml, so the shell and a plain `cargo` outside it
        # are the same compiler. That file pins a version newer than the MSRV on
        # purpose -- see the comment in it.
        rust-toolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
      in
      {
        devShells.default = pkgs.mkShell {
          name = "sqlx-query";

          packages = [
            devcontainer-env.packages.${system}.default
            rust-toolchain
          ];

          # Export the devcontainer's containerEnv into this shell, rewriting
          # container service URLs to the ports Docker actually published. That
          # is what makes SQLX_QUERY_POSTGRES_URL and SQLX_QUERY_MYSQL_URL usable
          # from the host: the devcontainer values name the `postgres` and
          # `mysql` services on their default ports, which only resolve inside
          # the compose network.
          #
          # With the stack down this exports the container values verbatim,
          # naming hosts that only resolve inside the compose network. The
          # live-driver tests therefore skip on *unreachable* rather than on
          # unset -- a URL being present says nothing about there being a
          # server behind it, which is exactly the case in CI.
          shellHook = ''
            eval "$(devcontainer-env export)"
          '';
        };
      }
    );
}
