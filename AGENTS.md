# verified

verified is the champäne of agents. It attempts to build a proven correct agent : https://chezdodds.dev/blog/verified-preregistration/.

It builds a modern agent harness oriented around safety and correctness. 

The project follows the traditional RFC process and this contains the record of design decisions in docs/rfcs.  

Prefer straightforward designs and delete accidental complexity rather than preserving it.

v controls **dispatch**: what runs, when, with whose approval, at most once, with exactly one recorded result. It does not control the filesystem or the OS. Tools act on the world best-effort and report honestly. Containment is a future sandbox `Env`.

This repository is a Rust workspace. Each crate under `crates/` sits on a real architectural boundary:

- `verified-domain`: the domain types every other crate shares: ids, records, exits and causes, tools, limits and the payload trait. It holds only types and pure helpers, written inside `verus!{}`, so the core's proofs can talk about them.
- `verified-core`: the sans-io core (`decide`, `apply`, `recover`). It is pure, sync and written inside `verus!{}`, and `just verify` checks it with Verus.
- `verified-wire`: the types the daemon and TUI share: commands, ids, AI SDK UI chunks, the transcript fold, SSE and Durable Streams.
- `verified-server`: the daemon. It holds the async shell, the seams (model, log, environment, clock, entropy) and the HTTP routes.
- `verified-tui`: the terminal client, an Elm-style loop that follows the daemon's stream.
- `verified-sim`: deterministic simulation tests on turmoil. Only this crate enables the `sim` feature.
- `verified-cli`: the one binary, `verified`, which runs the TUI or the daemon (`verified daemon`).

Add a crate only when the code has a new boundary of this kind; do not create speculative crates. Every crate must inherit workspace package metadata and lint configuration.

Nondeterminism enters through explicit seams: `clippy.toml` bans direct use of wall and monotonic time, randomness, the filesystem, environment variables, hash-ordered collections, and unannotated interior mutability. Pass these in as data or through a seam; when an exception is genuinely needed, use `#[expect(..., reason = "...")]`, never `#[allow]`.

Run `just ci` before committing changes. Keep local and CI validation on the same path.

The development environment uses Rust Glancer rather than rust-analyzer. Glancer requires the `rust-src` toolchain component and is provided by devenv as `rust-glancer`.

devenv also provides Verus (`nix/verus.nix`). Verus pins its own rustc, separate from `rust-toolchain.toml`, and only verification runs on that toolchain. To upgrade Verus, bump the release and its hashes in `nix/verus.nix`, the `vstd` pin in `Cargo.toml`, and the toolchain in `devenv.nix` together.
