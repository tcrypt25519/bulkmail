# Bulkmail

![Build Status](https://github.com/tcrypt25519/bulkmail/actions/workflows/ci.yml/badge.svg)

A parallel transaction sender with pluggable chain adapters (Ethereum today, Solana planned).

## Docs

- [Architecture](ARCHITECTURE.md)
- [Problem Answers](docs/answers.md)
- [Original Design Notes](docs/design.md) *(superseded by ARCHITECTURE.md)*

## Features

- Handles sending transactions to a rapid EVM blockchain
- Sends multiple transactions concurrently
- Implements a priority system for transactions
- Includes a mechanism for replacing stuck transactions
- Manages nonces to ensure proper transaction ordering
- Late-binds nonces and gas prices to messages for flexible processing

## Prerequisites

- Rust (latest stable version)

## Getting Started

### Building the Project

```bash
cargo build
```

### Quick Start (Ethereum)

```rust
use bulkmail::{
    Chain, Eth, EthClient, EthFeeManager, EthReplayProtection, EthRetryStrategy, Message, Sender,
};
use alloy::primitives::Address;
use std::sync::Arc;

# async fn run(chain: Chain, signer: Address) -> Result<(), bulkmail::Error> {
let chain_arc: Arc<dyn bulkmail::LegacyChainClient> = Arc::new(chain.clone());
let client = Arc::new(EthClient::new(chain_arc.clone()));
let fees = Arc::new(EthFeeManager::new());
let replay = Arc::new(EthReplayProtection::new(chain_arc, signer).await?);
let retry = Arc::new(EthRetryStrategy::new());

let sender = Sender::<Eth>::new(client, fees, replay, retry);
sender.add_message(Message::default()).await;
sender.run().await
# }
```

## Running the Oracle Simulation

Bulkmail includes an `oracle` program that simulates the system's behavior. To run it:

```bash
cargo run --example oracle_eth
```

## Running Tests

```bash
cargo test
```

## Project Structure

- `src/`: Contains the main library code
- `examples/`: Example programs (e.g., `oracle_eth.rs`)
