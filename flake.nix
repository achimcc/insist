{
  description = "Keep alerts open and escalating until a human acknowledges them";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs =
    { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAll = f: nixpkgs.lib.genAttrs systems (s: f nixpkgs.legacyPackages.${s});
    in
    {
      packages = forAll (pkgs: {
        default = pkgs.rustPlatform.buildRustPackage {
          pname = "insist";
          # Read out of Cargo.toml so the store path and the crate cannot disagree.
          version = (nixpkgs.lib.importTOML ./Cargo.toml).package.version;
          src = self;
          cargoLock.lockFile = ./Cargo.lock;
          # aws-lc-sys (reqwest's rustls feature) builds C code with cmake.
          nativeBuildInputs = [ pkgs.cmake ];
          # rustls-platform-verifier reads the system store as soon as a Client
          # is built, even in tests that only speak plain HTTP to a mock.
          SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
          meta = {
            description = "Keep alerts open and escalating until a human acknowledges them";
            license = pkgs.lib.licenses.agpl3Only;
            mainProgram = "insist";
          };
        };
      });

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [ cargo rustc rustfmt clippy cmake prometheus-alertmanager ntfy-sh python3 mkpasswd jq curl ];
          SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
        };
      });

      nixosModules.default = ./nix/module.nix;

      checks = forAll (pkgs: {
        package = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        clippy = self.packages.${pkgs.stdenv.hostPlatform.system}.default.overrideAttrs (old: {
          pname = "insist-clippy";
          nativeBuildInputs = old.nativeBuildInputs ++ [ pkgs.clippy ];
          buildPhase = "cargo clippy --all-targets -- -D warnings";
          installPhase = "touch $out";
        });
        fmt = self.packages.${pkgs.stdenv.hostPlatform.system}.default.overrideAttrs (old: {
          pname = "insist-fmt";
          nativeBuildInputs = old.nativeBuildInputs ++ [ pkgs.rustfmt ];
          buildPhase = "cargo fmt --check";
          installPhase = "touch $out";
        });
        vm = import ./nix/test.nix {
          inherit pkgs;
          module = self.nixosModules.default;
          package = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        };
      });
    };
}
