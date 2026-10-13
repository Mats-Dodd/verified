# verified

verified is the champäne of agents. It attempts to build a proven correct agent : https://chezdodds.dev/blog/verified-preregistration/.

It builds a modern agent harness oriented around safety and correctness. 

The project follows the traditional RFC process and this contains the record of design decisions in docs/rfcs.  

Prefer straightforward designs and delete accidental complexity rather than preserving it.

v controls **dispatch**: what runs, when, with whose approval, at most once, with exactly one recorded result. It does not control the filesystem or the OS. Tools act on the world best-effort and report honestly. Containment is a future sandbox `Env`. `PRINCIPLES.md` sets out what v upholds and what it doesn't, and the admission rule a new promise must pass. Read it before adding or changing any guarantee.

This repository is a Rust workspace. Each crate under `crates/` sits on a real architectural boundary:

- `verified-domain`: the domain types every other crate shares: ids, records, exits and causes, tools, limits and the payload trait. It holds types and pure helpers, written inside `verus!{}` so the core's proofs can talk about them, and α for records (`alpha`), plain Rust outside `verus!{}` for the model.
- `verified-core`: the sans-io core (`decide`, `apply`, `recover`). It is pure, sync and written inside `verus!{}`, and `just verify` checks it with Verus.
- `verified-model`: the Stateright model of the core in an abstract world (`Harness`). BFS explores it over a small domain (`Bounded`, which also bounds the world's faults); trace mode (`Harness::trace`) takes its actions' ids and sizes from a trace, and `replay` walks a run's observations through it one state at a time (each observation names the actions it means, and every `always` property is checked on each state). It also holds α for events and effects (α for records is `verified_domain::alpha`, plain Rust outside `verus!{}`), the abstraction from any payload to the model's `Abstract`. Its `suite` holds the bounds every model check runs under, and `coverage` keys each step of the model small (a call's class, how far it got and how it resolved; a crash against a run's act; a few situations) for BFS's edges (`coverage::explore`) and a replay's steps alike. Its tests hold the model checks, their planted bugs, and the property test that the core commutes with α.
- `verified-wire`: the types the daemon and TUI share: commands, ids, AI SDK UI chunks, the transcript fold, SSE and Durable Streams.
- `verified-server`: the daemon. It holds the async shell, the seams (model, log, environment, clock, entropy) and the HTTP routes.
- `verified-tui`: the terminal client, an Elm-style loop that follows the daemon's stream.
- `verified-sim`: deterministic simulation tests on turmoil. Only this crate enables the `sim` feature. Every run is also replayed through `verified-model`: the ledger, with the shell's sim-only observations (`shim::observe`), is folded into the model's `Observed` facts, and a run the model cannot follow fails. The replay checks the offsets the TUI read and when; what it folded there is the invariant I6's to check. `just coverage A B` prints the coverage keys BFS reaches under the suite's bounds against those seeds `A..A+B` and the directed scenarios reach, each gap classified (over-approximation, fidelity gap, test gap). A directed scenario (`directed`) is a literal scenario that reaches a gap no seed does: a BFS witness written out as the user's turns and answers, with the one ordering no seed promises, two runs acting together, kept by a barrier (`Scenario::ran_together`). `directed::run` fails one whose replay misses its target.
- `verified-cli`: the one binary, `verified`, which runs the TUI or the daemon (`verified daemon`).

Add a crate only when the code has a new boundary of this kind; do not create speculative crates. Every crate must inherit workspace package metadata and lint configuration.

Nondeterminism enters through explicit seams: `clippy.toml` bans direct use of wall and monotonic time, randomness, the filesystem, environment variables, hash-ordered collections, and unannotated interior mutability. Pass these in as data or through a seam; when an exception is genuinely needed, use `#[expect(..., reason = "...")]`, never `#[allow]`.

Run `just ci` before committing changes. Keep local and CI validation on the same path.

The development environment uses Rust Glancer rather than rust-analyzer. Glancer requires the `rust-src` toolchain component and is provided by devenv as `rust-glancer`.

devenv also provides Verus (`nix/verus.nix`). Verus pins its own rustc, separate from `rust-toolchain.toml`, and only verification runs on that toolchain. To upgrade Verus, bump the release and its hashes in `nix/verus.nix`, the `vstd` pin in `Cargo.toml`, and the toolchain in `devenv.nix` together.
