# verified

verified is the champäne of agents. Prefer straightforward designs and delete accidental complexity rather than preserving it.

This repository is a Rust workspace. `crates/verified` is the library; `crates/verified-cli` is a thin binary (`verified`) over it. Add crates under `crates/` only when the code has a real architectural boundary; do not create speculative crates. Every crate must inherit workspace package metadata and lint configuration.

Nondeterminism enters through explicit seams: `clippy.toml` bans direct use of wall and monotonic time, randomness, the filesystem, environment variables, hash-ordered collections, and unannotated interior mutability. Pass these in as data or through a seam; when an exception is genuinely needed, use `#[expect(..., reason = "...")]`, never `#[allow]`.

Run `just ci` before committing changes. Keep local and CI validation on the same path.

The development environment uses Rust Glancer rather than rust-analyzer. Glancer requires the `rust-src` toolchain component and is provided by devenv as `rust-glancer`.
