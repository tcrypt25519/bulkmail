Bulkmail Agents Guide
======================

Overview
--------
This repository contains a parallel transaction sender with pluggable chain adapters.
The current production implementation targets **Ethereum (EIP-1559)** only. A Solana
adapter is actively in progress, but **Solana is not yet supported** in the shipped
runtime.

Current Structure
-----------------
- `src/adapter/` defines the chain-agnostic traits and the Ethereum adapter.
- `src/sender.rs` is the orchestrator that manages queueing, retries, and confirmation.
- `src/message.rs` models message intent with deadline and priority behavior.
- `src/nonce_manager.rs` handles replay protection for Ethereum (nonces).
- `src/gas_price.rs` handles priority fee and congestion modeling.

ETH-Only Today (Solana In Progress)
-----------------------------------
ETH is the only supported chain today. The Solana work is being developed to align
with the existing adapter interfaces and retry semantics, but it is not part of the
runtime yet. This means any chain-specific logic must continue to preserve Ethereum
behavior first.

Key Differences vs Typical Implementations
-------------------------------------------
- **Late binding of nonce and fee**: Nonces and fees are bound immediately before
  submission, not at message creation. This enables clean retries and re-queues.
- **Clock-aware prioritization**: Message priority is time-sensitive; the priority
  queue captures effective priority at insertion time using a clock source.
- **Replay token release**: Failed or abandoned sends must release replay tokens
  (nonces) so they can be reused safely.
- **SendOutcome-based flow**: Send paths return `Confirmed`, `Dropped`, or `Reverted`
  and all retry handling is driven from that explicit outcome.

Context7 Library IDs
--------------------
Use these Context7 IDs when retrieving official docs/examples:
- `websites/solana`
- `solana-labs/solana`
- `websites/rs_solana-sdk_solana_sdk`
- `anza-xyz/solana-sdk`

Testing and Development
-----------------------
The canonical command for running manual P2P tests is located in the `test.sh`
script in the repository root. This script must always be kept up to date
whenever the required features, environment variables, or CLI arguments change.

Run it with:
```bash
./test.sh
```
Additional arguments passed to `./test.sh` will be forwarded to the `cli_tool` example.
