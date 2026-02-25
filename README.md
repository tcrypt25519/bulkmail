# Bulkmail

![Build Status](https://github.com/tyler-smith/bulkmail/actions/workflows/ci.yml/badge.svg)

A parallel transaction sender for Ethereum, designed to handle concurrent transaction sending to a rapid EVM blockchain.

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

### Cloning the Repository

To get started with Bulkmail, clone the repository to your local machine:

```bash
git clone https://github.com/tyler-smith/bulkmail.git
cd bulkmail
```

### Building the Project

```bash
cargo build
```

## Running the Oracle Simulation

Bulkmail includes an `oracle` program that simulates the system's behavior. To run it:

```bash
cargo run --bin oracle
```

## Running Tests

```bash
cargo test
```

## Project Structure

- `src/`: Contains the main library code
- `src/bin/`: Contains the `oracle` simulation program
