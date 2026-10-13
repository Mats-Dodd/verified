default:
    just --list

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all --check

# Two configurations, each checked: everything with the `sim` shims on
# (`--all-features`, which verified-cli refuses to compile), and the shipped
# crates with the real IO (tests/local.rs: process groups, timeouts, FileLog's
# torn tail on a real disk, flock), which `--all-features` compiles out.
real := "-p verified-server -p verified-tui -p verified-cli"

check: fmt-check clippy
    cargo check --workspace --exclude verified-cli --all-targets --all-features
    cargo check {{real}} --all-targets

clippy:
    cargo clippy --workspace --exclude verified-cli --all-targets --all-features -- -D warnings
    cargo clippy {{real}} --all-targets -- -D warnings

test:
    cargo nextest run --workspace --exclude verified-cli --all-features --no-tests pass
    cargo nextest run {{real}} --no-tests pass

# The simulation (verified-sim) runs first, in `sim`.
test-ci:
    cargo nextest run --workspace --exclude verified-sim --exclude verified-cli --all-features --profile ci --no-tests pass
    cargo nextest run {{real}} --profile ci --no-tests pass

# Deterministic simulation first: every fixed seed runs twice and the two
# traces must match before any world invariant is judged, then the same seeds
# run in two fresh processes (randomness drawn per process would make a
# failing seed impossible to replay). The nightly sweep is `just sweep`; see
# a seed's trace with `cargo run -p verified-sim -- trace --seed N`.
sim:
    cargo nextest run -p verified-sim --profile ci
    cargo run --quiet -p verified-sim -- processes

# Many seeds, each run twice, optimized but unwinding (`sim-release`: a
# daemon panic crashes its simulated host and fails the seed, where the
# release profile's `panic = "abort"` would end the sweep).
sweep from="0" count="1000":
    cargo run --quiet --profile sim-release -p verified-sim -- run --from {{from}} --count {{count}}

# Model coverage: the coverage keys the seeds reach (each run replayed
# through the model) against those BFS reaches under the model checks'
# bounds, and each gap classified (over-approximation, fidelity, test).
coverage from="0" count="200":
    cargo run --quiet --profile sim-release -p verified-sim -- coverage --from {{from}} --count {{count}}

# Each of `count` seeds from `from` traced in two fresh processes.
processes from count:
    cargo run --quiet --profile sim-release -p verified-sim -- processes --from {{from}} --count {{count}}

# The shipped binary never links the simulation shims (the `sim` features of
# verified-server and verified-tui, and so turmoil). Builds the root's
# default members in release, as a release build does (verified-cli refuses
# to compile with the shims on), then checks the features Cargo resolved for
# that same set. `-p verified-cli` alone would resolve features for the
# binary alone and never see a leak through the workspace.
release-without-sim:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --release
    # `cargo tree` must succeed on its own: a failure here must fail the
    # recipe, not read as "no leak" through the pipe.
    tree="$(cargo tree --edges normal,features --prefix none)"
    if grep -E 'feature "sim"|turmoil' <<<"$tree"; then
        echo "the release build links the simulation shims" >&2
        exit 1
    fi

doctest:
    cargo test --workspace --doc

docs:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps

deny:
    cargo deny check

unused-deps:
    cargo machete crates

supply-chain: deny unused-deps

# Verify verified-domain and verified-core with Verus release
# 0.2026.10.04.426d8b0 (the core's proofs mention the domain's types); vstd in
# Cargo.toml is pinned to match and cargo-verus refuses a mismatched pair.
# devenv provides `verus` and `cargo-verus` (nix/verus.nix) wired to their own
# rustc 1.98.1, separate from the workspace toolchain; no rustup needed.
# $VERUS_BIN_DIR overrides with an unpacked release, which then needs `rustup`
# with toolchain 1.98.1. Output lands in target/verus-partial, apart from
# normal builds.
verify:
    PATH="${VERUS_BIN_DIR:+$VERUS_BIN_DIR:}$PATH" cargo verus verify -p verified-domain -p verified-core

ci: sim check test-ci doctest docs verify supply-chain release-without-sim
