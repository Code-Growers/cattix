{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/master";
    make-shell.url = "github:nicknovitski/make-shell";

    fenix = {
      url = "github:nix-community/fenix/monthly";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    crane = {
      url = "github:ipetkov/crane";
    };
  };

  outputs =
    inputs@{
      self,
      nixpkgs,
      flake-parts,
      systems,
      make-shell,
      fenix,
      crane,
      ...
    }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      imports = [ make-shell.flakeModules.default ];
      flake = {
        nixosModules.default = import ./nix/cattix-module.nix;
        nixosModules.haproxy = import ./nix/extensions/haproxy.nix;
        lib.mkFleet = import ./nix/cattix-lib.nix { inherit (nixpkgs) lib; };
      };
      systems = [
        "x86_64-linux"
        "aarch64-darwin"
      ];

      perSystem =
        {
          config,
          self',
          inputs',
          pkgs,
          system,
          ...
        }:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          openssl_pkgs = pkgs.openssl;
          fenixSys = fenix.packages.${system};
          craneLib = (crane.mkLib pkgs).overrideToolchain fenixSys.minimal.toolchain;
          package = craneLib.buildPackage {
            pname = "cattix";
            version = "0.1.0";
            src = craneLib.cleanCargoSource ./.;
            nativeBuildInputs = [
              pkgs.makeWrapper
              pkgs.sccache
              pkgs.clang
              pkgs.mold
              pkgs.pkg-config
            ];
            buildInputs = [ openssl_pkgs.dev ];
            postInstall = ''
              wrapProgram $out/bin/cattix \
                --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.nix pkgs.nvd ]}
            '';
            cargoBuildCommand = "cargo build --profile release --bins";
            cargoInstallCommand = "cargo install --path . --root $out --bins";
          };
          cattixVmTest = import ./tests/nixos/cattix-cli.nix {
            inherit pkgs package nixpkgs;
          };
          cattixVmLab = import ./tests/manual-vm-lab.nix {
            inherit pkgs nixpkgs;
          };
        in
        {
          checks = {
            inherit package;
            cattix-vm = cattixVmTest;
          };

          packages = {
            default = package;
            cattix-vm-lab = cattixVmLab;
          };

          make-shells.default = {
            env = {
              PKG_CONFIG_PATH = "${openssl_pkgs.dev}/lib/pkgconfig";
              LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
              LD_LIBRARY_PATH = pkgs.lib.strings.concatStrings (
                pkgs.lib.strings.intersperse ":" [
                  "$LD_LIBRARY_PATH"
                  "${openssl_pkgs.out}/lib"
                  "${pkgs.stdenv.cc.cc.lib}/lib"
                ]
              );
            };

            packages = [
              pkgs.pkg-config
              pkgs.openssl.dev
              pkgs.mold
              pkgs.clang
              pkgs.sccache
              pkgs.nvd
              pkgs.nodejs

              (fenixSys.complete.withComponents [
                "cargo"
                "clippy"
                "rust-src"
                "rustc"
                "rustfmt"
              ])
              fenixSys.rust-analyzer
            ];
          };
        };
    };
}
