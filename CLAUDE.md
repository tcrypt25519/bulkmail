# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
# Build
cargo build

# Run tests
cargo test

# Run a single test
cargo test <test_name>

# Run tests for a specific module
cargo test --lib <module>::tests

# Run the oracle simulation (requires Anvil in PATH)
cargo run --bin oracle

# Format code
cargo fmt

# Lint
cargo clippy
```

## Architecture

Bulkmail is a parallel Ethereum EIP-1559 transaction sender library. It manages concurrent transaction submission to a fast EVM chain, handling nonce sequencing, gas pricing, retries, and stuck-transaction replacement.

### Core Data Flow

```
Message → PriorityQueue → Sender.run() loop → NonceManager → GasPriceManager → Chain → Ethereum
```

1. Callers create `Message` values (intent to send; no nonce or gas price yet) and call `Sender::add_message`.
2. `Sender::run` drives a `tokio::select!` loop that reacts to new blocks (to re-sync the nonce) and calls `process_next_message`.
3. A `Semaphore` (`MAX_IN_FLIGHT_TRANSACTIONS = 16`) caps concurrency; each message is processed in its own `tokio::spawn` task.
4. Nonces are late-bound: `NonceManager::get_next_available_nonce` assigns the next unused nonce atomically using an in-flight `BTreeSet`.
5. Gas prices are late-bound: `GasPriceManager::get_gas_price` computes `(base_fee, priority_fee)` from recent confirmation-time history and message priority.
6. `Chain` wraps Alloy's WebSocket provider: signs EIP-1559 transactions and returns a `PendingTransactionBuilder` watcher.
7. If a transaction times out (`TX_TIMEOUT = 3s`), `handle_transaction_dropped` bumps the priority fee by 20% and retries (up to `MAX_REPLACEMENTS = 3`). Messages themselves retry up to `MAX_RETRIES = 3`.

### Module Map

| File | Role |
|------|------|
| `src/lib.rs` | Public API surface; top-level `Error` enum |
| `src/sender.rs` | Orchestrator; owns queue, pending map, semaphore |
| `src/message.rs` | `Message` struct; `effective_priority` calculation |
| `src/chain.rs` | Alloy WebSocket wrapper; signing and RPC calls |
| `src/nonce_manager.rs` | In-flight nonce tracking; per-block sync |
| `src/gas_price.rs` | Congestion detection; priority-fee scaling |
| `src/priority_queue.rs` | Max-heap over `effective_priority` |
| `src/bin/oracle.rs` | Simulation using Anvil; not part of the library |

### Key Design Decisions

- **Late binding**: Nonces and gas prices are assigned immediately before sending, not at message creation. This allows re-queueing and replacement without invalidating earlier assignments.
- **Priority scoring**: `effective_priority = base_priority + retry_count + age_factor + deadline_factor`. Deadline approaching within 2 blocks adds `MAX_PRIORITY (100)`; within 10 blocks adds `MAX_PRIORITY/3`.
- **Gas price adaptation**: `GasPriceManager` maintains a rolling window of 10 confirmation times. Congestion level (Low/Medium/High) multiplies the priority fee; message priority adds an additional 0–100% on top.
- **Nonce sync**: Every new block triggers `NonceManager::sync_nonce`, which reconciles local state against the chain's confirmed nonce.
- `Chain` requires a WebSocket URL (not HTTP) because it subscribes to new block headers.

### Oracle Binary

`src/bin/oracle.rs` is a self-contained simulation: it spawns a local Anvil node, creates a `Chain` + `Sender`, and continuously submits transfer messages at random intervals. It depends on `alloy-node-bindings` (dev-ish dependency listed in `[dependencies]`). Running it requires `anvil` to be installed.
