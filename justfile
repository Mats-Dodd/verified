default:
    just --list

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all --check

check: fmt-check clippy
    cargo check --workspace --all-targets --all-features

clippy:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

test:
    cargo nextest run --workspace --all-features --no-tests pass

test-ci:
    cargo nextest run --workspace --all-features --profile ci --no-tests pass

doctest:
    cargo test --workspace --doc

docs:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps

deny:
    cargo deny check

unused-deps:
    cargo machete crates

supply-chain: deny unused-deps

ci: check test-ci doctest docs supply-chain
