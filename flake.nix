{
  description = "Mattak supports semantic web applications in Axum";
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/25.11";
    # nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };
  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      rust-overlay,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = (
          import "${nixpkgs}" {
            inherit system overlays;
          }
        );

        runDeps = with pkgs; [
          openssl
        ];

        buildDeps =
          with pkgs;
          [
            pkg-config
          ]
          ++ runDeps;
      in
      {
        devShells.default = pkgs.mkShell {
          buildInputs =
            with pkgs;
            [
              # (rust-bin.selectLatestNightlyWith ( toolchain: toolchain.default))
              # .override { extensions = [ "rust-analyzer" ]; }
              (rust-bin.stable.latest.default.override {
                extensions = [
                  "rust-analyzer"
                  "rust-src"
                ];
              })
            ]
            ++ buildDeps;
        };
      }
    );
}
