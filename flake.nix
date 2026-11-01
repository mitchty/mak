{
  description = "mitchty/mak: Mitchty Army Knife, just a bunch of crap I decided shant live in shell";
  outputs =
    {
      self,
      flakelight,
      flakelight-rust,
      nixpkgs,
      ...
    }:
    let
      inherit (nixpkgs) lib;

      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      linuxSystems = builtins.filter (s: lib.hasSuffix "-linux" s) systems;

      rustFlake = flakelight ./. (
        { lib, ... }:
        {
          imports = [ flakelight-rust.flakelightModules.default ];
          inputs.self = self;

          inherit systems;

          # Provide a loosey goosey deny.toml setup.
          fileset = lib.fileset.unions [
            (lib.fileset.fileFilter (f: f.hasExt "rs" || f.name == "Cargo.toml") ./.)
            (./. + /Cargo.lock)
            ./deny.toml
          ];

          defaultBinary = "mak";

          binaries = {
            cilogger = {
              crate = ./crates/cilogger;
              pathDeps = [ ./crates/core ];

              variants = {
                default = { };
              };
            };

            ecahook = {
              crate = ./crates/ecahook;
              pathDeps = [ ];
            };

            mak = {
              crate = ./crates/mak;
              pathDeps = [ ./crates/core ];
            };
          };
        }
      );

      # Stolen from wip yeet flake.nix
      integrationChecksFor =
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          cilogger = rustFlake.packages.${system}.cilogger;

          integrationTestFiles = builtins.sort (a: b: a < b) (
            builtins.attrNames (
              lib.filterAttrs (
                name: type: type == "regular" && lib.hasPrefix "integration-" name && lib.hasSuffix ".nix" name
              ) (builtins.readDir ./nix)
            )
          );

          integrationChecks = lib.listToAttrs (
            map (
              file:
              let
                withoutPrefix = lib.removePrefix "integration-" file;
                withoutSuffix = lib.removeSuffix ".nix" withoutPrefix;
                matched = builtins.match "([0-9][0-9]-)(.+)" withoutSuffix;
                namePart = if matched != null then builtins.elemAt matched 1 else withoutSuffix;
              in
              {
                name = "mak-int-${namePart}";
                value = pkgs.callPackage (./nix + "/${file}") { inherit cilogger; };
              }
            ) integrationTestFiles
          );
        in
        integrationChecks;
    in
    lib.recursiveUpdate rustFlake {
      checks = lib.genAttrs linuxSystems integrationChecksFor;
    };
  inputs = {
    flakelight.url = "github:nix-community/flakelight";
    flakelight-rust.url = "github:mitchty/flakelight-rust";
    nixpkgs.follows = "flakelight-rust/nixpkgs";
  };
}
