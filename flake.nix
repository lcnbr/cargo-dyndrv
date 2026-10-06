{
  inputs = {
    nixpkgs.url = "https://channels.nixos.org/nixos-unstable/nixexprs.tar.xz";
  };

  outputs =
    {
      nixpkgs,
      self,
    }:
    let
      inherit (nixpkgs) lib;
      makePkgs =
        system:
        import nixpkgs {
          inherit system;
          overlays = [ self.overlays.default ];
        };
      forAllSystems = f: lib.genAttrs lib.systems.flakeExposed (system: f (makePkgs system));
    in
    {
      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            # Use nixpkgs rust, fenix rust uses an integrated ld that doesn't set proper rpaths
            rustc
            cargo
            rustfmt
            clippy

            gdb
            rust-analyzer
          ];

          # For some reason nixpkgs cargo will accept unstable features.
          # If it didn't we could use __CARGO_TEST_CHANNEL_OVERRIDE_DO_NOT_USE_THIS=nightly
          # or cargo (but not rustc) from fenix
          inherit (pkgs.cargo-dyndrv) BUILD_WRAP COMPILE_WRAP SOURCE_WRAP ENV_WRAP TARGET_ENV;
        };
        cross =
          let
            pkgs' = pkgs.pkgsCross.aarch64-multiplatform;
          in
          pkgs'.stdenv.mkDerivation rec {
            name = "shell-cross";
            nativeBuildInputs = with pkgs'; [
              rustc
              cargo
              rustfmt
              clippy

              gdb
              rust-analyzer
            ];

            HOST_CC = lib.getExe pkgs'.buildPackages.stdenv.cc;
            HOST_CXX = lib.getExe' pkgs'.buildPackages.stdenv.cc "${pkgs'.buildPackages.stdenv.cc.targetPrefix}c++";

            "CARGO_TARGET_${pkgs'.stdenv.buildPlatform.rust.cargoEnvVarTarget}_LINKER" = HOST_CC;
            "CARGO_TARGET_${pkgs'.hostPlatform.rust.cargoEnvVarTarget}_LINKER" = lib.getExe pkgs'.stdenv.cc;

            inherit (pkgs'.cargo-dyndrv) BUILD_WRAP COMPILE_WRAP SOURCE_WRAP ENV_WRAP TARGET_ENV;
          };
      });

      overlays.default =
        final: prev:
        let
          # these only use stdlib, no need to use cargo
          makeHelper =
            name:
            final.buildPackages.buildRustCrate {
              crateName = name;
              version = "0.1.0";
              edition = (builtins.fromTOML (builtins.readFile ./${name}/Cargo.toml)).package.edition;
              src = ./${name};
              crateBin = [ { inherit name; } ];
            };
          baseArgs = rec {
            pname = "cargo-dyndrv";
            version = "0.1.0";

            src = lib.fileset.toSource {
              root = ./.;
              fileset = lib.fileset.unions [
                ./Cargo.toml
                ./Cargo.lock
                ./cargo-dyndrv
                # cargo insists on seeing all members of a workspace,
                # even when resolving only one of them.
                ./env-wrap
                ./target-env
                ./build-wrap
                ./compile-wrap
                ./source-wrap
                ./compiler-recipe
              ];
            };
            cargoLock = {
              lockFile = ./Cargo.lock;
              outputHashes."harmonia-file-core-3.1.0" = "sha256-hPdQB+DWc/q6w/wnTVGCS8iugEGd60Ym3x2eWvVw3ss=";
            };

            cargoBuildFlags = [
              "--bin"
              pname
            ];

            env = {
              BUILD_WRAP = lib.getExe (makeHelper "build-wrap");
              SOURCE_WRAP = lib.getExe (makeHelper "source-wrap");
              COMPILE_WRAP = lib.getExe (makeHelper "compile-wrap");
              ENV_WRAP = lib.getExe (makeHelper "env-wrap");
              TARGET_ENV = lib.getExe (makeHelper "target-env");

              LN = lib.getExe' final.buildPackages.coreutils "ln";
            };

            meta.mainProgram = "cargo-dyndrv";
          };
        in
        {
          cargo-dyndrv = final.rustPlatform.buildRustPackage baseArgs;
          cargo-dyndrv-dyn = final.buildDynamicCrate (
            baseArgs
            // {
              outputs = [ "cargo-dyndrv" ];
            }
          );

          writeExtern = final.callPackage ./nix/write-extern.nix { };
          buildDynamicCrate = final.callPackage ./nix/build-crate.nix { };
        };

      packages = forAllSystems (pkgs: {
        inherit (pkgs)
          cargo-dyndrv
          cargo-dyndrv-dyn
          writeExtern
          buildDynamicCrate
          ;

        cargo-dyndrv-dyn-cross = pkgs.pkgsCross.aarch64-multiplatform.cargo-dyndrv-dyn;
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt-tree);
    };
}
