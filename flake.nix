{
  description = "Baedeker - Substrate chain testing framework";
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/release-24.11";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-parts = {
      url = "github:hercules-ci/flake-parts";
      inputs.nixpkgs-lib.follows = "nixpkgs";
    };
    crane.url = "github:ipetkov/crane";
    shelly = {
      url = "github:CertainLach/shelly";
      inputs.flake-parts.follows = "flake-parts";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };
  outputs = inputs: inputs.flake-parts.lib.mkFlake {inherit inputs;} {
      imports = [inputs.shelly.flakeModule];
      systems = inputs.nixpkgs.lib.systems.flakeExposed;
      perSystem = {
        pkgs,
        system,
        ...
      }: let
        rust = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
        craneLib = (inputs.crane.mkLib pkgs).overrideToolchain rust;
      in {
        _module.args.pkgs = import inputs.nixpkgs {
          inherit system;
          overlays = [inputs.rust-overlay.overlays.default];
        };
        packages = rec {
          default = baedeker;
          baedeker = pkgs.callPackage ./nix/baedeker.nix {inherit craneLib;};
          baedeker-static = baedeker.override {static = true;};
        };
        shelly.shells.default = {
          factory = craneLib.devShell;
          packages = with pkgs; [
            cargo-edit
            rustPlatform.bindgenHook
          ];

          environment.PROTOC = "${pkgs.protobuf}/bin/protoc";
        };
      };
    };
}
