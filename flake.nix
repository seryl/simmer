{
  description = "simmer — instant Nix devshells: evaluate once, replay natively in microseconds";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane.url = "github:ipetkov/crane";
  };

  outputs = {
    self,
    nixpkgs,
    fenix,
    crane,
  }: let
    forSystems = nixpkgs.lib.genAttrs [
      "x86_64-linux"
      "aarch64-linux"
      "x86_64-darwin"
      "aarch64-darwin"
    ];

    craneFor = system:
      (crane.mkLib nixpkgs.legacyPackages.${system})
      .overrideToolchain fenix.packages.${system}.stable.minimalToolchain;

    # crane's default filter keeps only Cargo-relevant files; the warm
    # harness shim is embedded via include_str!("shim.bash") and must
    # survive too.
    srcFor = craneLib:
      nixpkgs.lib.cleanSourceWith {
        src = ./.;
        filter = path: type:
          craneLib.filterCargoSources path type
          || builtins.match ".*\\.bash$" path != null;
      };
  in {
    packages = forSystems (system: let
      craneLib = craneFor system;
      simmer = craneLib.buildPackage {
        pname = "simmer";
        version = "0.1.0";
        src = srcFor craneLib;
        strictDeps = true;
        meta = {
          description = "Instant Nix devshells: cached environments replayed natively into fish/bash/zsh";
          mainProgram = "simmer";
        };
      };
    in {
      default = simmer;
      inherit simmer;
    });

    checks = forSystems (system: let
      craneLib = craneFor system;
      src = srcFor craneLib;
      cargoArtifacts = craneLib.buildDepsOnly {inherit src;};
    in {
      clippy = craneLib.cargoClippy {
        inherit src cargoArtifacts;
        cargoClippyExtraArgs = "--all-targets -- --deny warnings";
      };
      test = craneLib.cargoTest {inherit src cargoArtifacts;};
    });

    devShells = forSystems (system: let
      pkgs = nixpkgs.legacyPackages.${system};
      toolchain = fenix.packages.${system}.stable.withComponents [
        "cargo"
        "clippy"
        "rust-src"
        "rustc"
        "rustfmt"
      ];
    in {
      default = pkgs.mkShell {
        packages = [
          toolchain
          fenix.packages.${system}.rust-analyzer
        ];
      };
    });
  };
}
