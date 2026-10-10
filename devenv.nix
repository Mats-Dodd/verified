{ pkgs, ... }:

let
  rust-glancer = pkgs.callPackage ./nix/rust-glancer.nix { };
in
{
  name = "verified";

  languages.rust = {
    enable = true;
    toolchainFile = ./rust-toolchain.toml;
  };

  packages = with pkgs; [
    just
    cargo-nextest
    cargo-deny
    cargo-machete
    rust-glancer
  ];

  env.CARGO_TERM_COLOR = "always";

  git-hooks.hooks = {
    rustfmt = {
      enable = true;
      settings.check = true;
    };

    clippy = {
      enable = true;
      settings = {
        allFeatures = true;
        denyWarnings = true;
      };
    };
  };

  tasks."verified:ci" = {
    exec = "just ci";
    before = [ "devenv:enterTest" ];
  };
}
