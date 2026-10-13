{ pkgs, inputs, ... }:

let
  rust-glancer = pkgs.callPackage ./nix/rust-glancer.nix { };
  # Verus pins its own rustc (1.98.1 for this release), separate from the
  # workspace toolchain; only verification runs on it.
  verus = pkgs.callPackage ./nix/verus.nix {
    rustToolchain = (inputs.rust-overlay.lib.mkRustBin { } pkgs).stable."1.98.1".minimal;
  };
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
    verus
  ];

  env.CARGO_TERM_COLOR = "always";

  git-hooks.hooks = {
    rustfmt = {
      enable = true;
      settings.check = true;
    };

    # Clippy runs through `just clippy`: the workspace cannot be linted with
    # one `--all-features` pass, because the `sim` shims compile out the real
    # IO. The recipe lints the sim and real-IO configurations separately, and
    # CI uses the same recipe.
    clippy = {
      enable = true;
      name = "clippy";
      entry = "${pkgs.just}/bin/just clippy";
      files = "\\.rs$|Cargo\\.(toml|lock)$|clippy\\.toml$";
      pass_filenames = false;
    };
  };

  tasks."verified:ci" = {
    exec = "just ci";
    before = [ "devenv:enterTest" ];
  };
}
