{
  description = "peck: Vimium-style mouseless navigation for Niri";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { nixpkgs, ... }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = f:
        nixpkgs.lib.genAttrs systems
          (system: f { inherit system; pkgs = nixpkgs.legacyPackages.${system}; });

      nativeBuildInputs = pkgs: [ pkgs.pkg-config ];
      buildInputs = pkgs: [ pkgs.wayland pkgs.libxkbcommon ];

      peckFor = pkgs: pkgs.rustPlatform.buildRustPackage {
        pname = "peck";
        version = "0.1.0";
        src = ./.;
        cargoLock = {
          lockFile = ./Cargo.lock;
          # niri-ipc is a git dependency (PR #4147); importCargoLock needs its
          # vendored-source hash.
          outputHashes = {
            "niri-ipc-26.4.0" = "sha256-3HxntJA2DNg+L94gbM86uGeJqPPWbSX88CjteOmYV0o=";
          };
        };
        nativeBuildInputs = nativeBuildInputs pkgs;
        buildInputs = buildInputs pkgs;
        meta.mainProgram = "peck";
      };
    in {
      packages = forAllSystems ({ pkgs, ... }: {
        default = peckFor pkgs;
      });

      devShells = forAllSystems ({ pkgs, ... }: {
        default = pkgs.mkShell {
          nativeBuildInputs = nativeBuildInputs pkgs;
          buildInputs = buildInputs pkgs;
          packages = with pkgs; [
            cargo
            rustc
            clippy
            rustfmt
            rust-analyzer
            # utilities for inspecting accessibility trees
            accerciser
            at-spi2-core
          ];
          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath (buildInputs pkgs);
        };
      });

      checks = forAllSystems ({ pkgs, ... }: {
        build = peckFor pkgs;

        clippy = (peckFor pkgs).overrideAttrs (old: {
          pname = "${old.pname}-clippy";
          nativeBuildInputs = old.nativeBuildInputs ++ [ pkgs.clippy ];
          buildPhase = "cargo clippy --all-targets -- --deny warnings";
          installPhase = "touch $out";
          doCheck = false;
        });

        fmt = pkgs.runCommand "peck-fmt"
          { nativeBuildInputs = [ pkgs.cargo pkgs.rustfmt ]; } ''
          cd ${./.}
          cargo fmt --check
          touch $out
        '';
      });
    };
}
