{
  description = "Repository-first grading platform development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      nixpkgs,
      flake-utils,
      rust-overlay,
      ...
    }:
    flake-utils.lib.eachSystem
      [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ]
      (
        system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ rust-overlay.overlays.default ];
          };
          myRust = pkgs.rust-bin.stable."1.98.1".default.override {
            extensions = [
              "clippy"
              "rust-analyzer"
              "rust-src"
              "rustfmt"
            ];
          };
          rustPlatform = pkgs.makeRustPlatform {
            cargo = myRust;
            rustc = myRust;
          };
          application = rustPlatform.buildRustPackage {
            pname = "grading-portal";
            version = "0.1.0";
            src = pkgs.lib.cleanSourceWith {
              src = ./.;
              filter =
                path: type:
                !(builtins.elem (baseNameOf path) [
                  ".git"
                  ".direnv"
                  ".local"
                  "target"
                  "result"
                ])
                && pkgs.lib.cleanSourceFilter path type;
            };
            cargoLock.lockFile = ./Cargo.lock;
            SQLX_OFFLINE = "true";
            SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
            nativeBuildInputs = [
              pkgs.pkg-config
              pkgs.removeReferencesTo
            ];
            buildInputs = [ pkgs.openssl ];
            doCheck = true;
            postFixup = ''
              find "$out/bin" -type f -exec remove-references-to -t ${myRust} '{}' +
            '';
            disallowedReferences = [ myRust ];
          };
          images = import ./nix/images.nix { inherit pkgs application; };
        in
        {
          formatter = pkgs.nixfmt;

          packages = {
            default = application;
            grading-portal = application;
          }
          // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux images;

          checks.build = application;

          devShells.default = pkgs.mkShell {
            packages = with pkgs; [
              myRust
              bash
              cargo-nextest
              curl
              git
              jq
              just
              kubectl
              nixfmt
              openssl
              pkg-config
              postgresql_18
              python3
              skopeo
              sqlx-cli
            ];

            RUST_SRC_PATH = "${myRust}/lib/rustlib/src/rust/library";
            RUST_BACKTRACE = "1";
            SQLX_OFFLINE = "true";
          };
        }
      );
}
