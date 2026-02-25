# Copilot instructions for `tcrypt25519/bulkmail`

## What this repository is
- `bulkmail` is a Rust library for concurrent EIP-1559 transaction submission on EVM chains.
- Main orchestration is in `src/sender.rs`, with chain I/O in `src/chain.rs` and message/priority/nonce/gas helpers in sibling modules.
- Public API surface is re-exported from `src/lib.rs`.

## Fast codebase orientation
- Read first:
  1. `README.md` (build/test basics)
  2. `docs/design.md` (architecture and lifecycle)
  3. `src/lib.rs` docs and exported modules
- Key files:
  - `src/sender.rs`: send loop, retries, replacement logic, concurrency limit
  - `src/chain.rs`: `ChainClient` trait + concrete `Chain` implementation
  - `src/message.rs`, `src/priority_queue.rs`, `src/nonce_manager.rs`, `src/gas_price.rs`
  - `src/bin/oracle.rs`: local simulation using Anvil bindings

## Build, test, lint
- Local baseline commands:
  - `cargo build`
  - `cargo test`
  - `cargo clippy`
- CI workflow (`.github/workflows/ci.yml`) additionally uses:
  - `cargo nextest run`
  - `cargo llvm-cov --lcov --output-path lcov.info`

## Testing and change guidance
- Keep changes minimal and module-scoped.
- Prefer testing behavior through existing unit tests in module files.
- For sender/nonce/chain interactions, use the `ChainClient` trait abstraction (`src/chain.rs`) and mock implementation (`MockChainClient` under `#[cfg(test)]`) instead of introducing real RPC dependencies in tests.
- Preserve existing async patterns (`tokio`, `async_trait`) and error types (`thiserror` enums in `src/lib.rs` and `src/chain.rs`).

## Known errors/workarounds observed during onboarding
- Local run of `cargo nextest run` failed with `error: no such command: nextest` (tool not installed in this environment).
  - Workaround used: run `cargo test` locally, or install nextest first (`cargo install cargo-nextest`) to match CI.
- A GitHub Actions run returned `conclusion: action_required` with `0` jobs, and fetching run logs URL returned `404`.
  - Workaround used: validate locally with `cargo build`, `cargo test`, and `cargo clippy` until the workflow can execute jobs.
