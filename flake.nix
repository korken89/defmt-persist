{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    # Packaged Kani, so `cargo kani` works without `cargo kani setup`.
    nix-tools = {
      url = "github:gleachkr/nix-tools";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };
  outputs =
    {
      nixpkgs,
      fenix,
      nix-tools,
      ...
    }:
    let
      # The nix-tools Kani only supports x86_64-linux.
      systems = [ "x86_64-linux" ];

      forAllSystems =
        f:
        nixpkgs.lib.genAttrs systems (
          system:
          f {
            inherit system;
            pkgs = nixpkgs.legacyPackages.${system};
            fenix = fenix.packages.${system};
          }
        );
    in
    {
      formatter = forAllSystems ({ pkgs, ... }: pkgs.nixfmt-rfc-style);

      devShells = forAllSystems (
        {
          system,
          pkgs,
          fenix,
        }:
        {
          default = pkgs.mkShell {
            packages = [
              # Stable Rust, pinned by flake.lock.
              (fenix.combine [
                (fenix.stable.withComponents [
                  "cargo"
                  "clippy"
                  "rust-src"
                  "rustc"
                  "rustfmt"
                ])
                fenix.targets.thumbv7m-none-eabi.stable.rust-std
              ])
              # `cargo xtask test`
              pkgs.qemu
              # `cargo kani`
              nix-tools.packages.${system}.kani
            ];
          };
        }
      );
    };
}
