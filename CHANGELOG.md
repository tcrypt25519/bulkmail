# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- CI coverage job using `cargo llvm-cov` with Codecov upload.
- `ChainClient` trait and channel-backed block subscription to enable mocking in tests.

### Changed
- Updated core dependencies (including `alloy` 1.7, `tokio`, `secp256k1`, `thiserror`, and `ethereum-types`) and refreshed `Cargo.lock`.
- `Sender` now consumes a channel-backed block stream and uses the `ChainClient` abstraction.
- `Message.gas` uses `u64` instead of `u128`, and message priority is clamped to the maximum.
- Expanded error taxonomy in `src/lib.rs` for clearer diagnostics.

### Improved
- Added documentation and clarifying comments across core modules (`chain`, `sender`, `gas_price`, `message`, `nonce_manager`, `priority_queue`).
- Simplified CI job steps by using direct `cargo` commands and updated action versions.
