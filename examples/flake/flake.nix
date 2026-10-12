{
  description = "simmer example: a small dev shell";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = {nixpkgs, ...}: let
    systems = ["x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin"];
    forAll = nixpkgs.lib.genAttrs systems;
  in {
    devShells = forAll (system: let
      pkgs = nixpkgs.legacyPackages.${system};
    in {
      default = pkgs.mkShell {
        packages = [pkgs.hello pkgs.jq];
        GREETING = "hello from simmer";
        # Runs once, when simmer builds the environment — not at every
        # prompt. Its exports become part of the cached environment.
        shellHook = ''
          export SHELL_BUILT_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
        '';
      };
    });
  };
}
