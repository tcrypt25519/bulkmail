# Mempool Monitoring Status

Date: 2026-04-24

## Current State

The P2P runtime builds and starts both Ethereum network stacks. The Consensus
Layer path has connected to mainnet peers in live runs and received beacon block
and finality traffic. The Execution Layer path now gets past the previous
bootnode parsing failure, performs discovery, records discovered peers, and can
prime future starts from the local execution peer seed cache.

The remaining blocker is sustained Execution Layer gossip. Live runs have shown
EL peers being discovered, added, briefly activated, and then removed, but not
stable EL peer sessions or durable pending transaction flow.

## Verified

- `cargo test -p mempooloracle --features 'reth-p2p consensus-p2p' --tests`
  passes.
- `./test.sh` delegates to the canonical `scripts/test-mempool.sh` command.
- Consensus peers connect in live runs and finality updates arrive.
- Execution discovery emits peer candidates and writes them to the local seed
  cache at the system temp path:
  `mempooloracle-execution-seeds-<chain>.txt`.
- Subsequent runs prime cached EL peers and audit lines include
  `PrimeConnect`, `SessionActive`, `SessionClosed`, and `PeerRemoved`.

## Open Problems

- EL peers are not staying connected long enough to produce steady transaction
  gossip.
- The audit log records session lifecycle events, but failed outbound dials and
  protocol-level disconnect reasons are still mostly hidden inside `reth` trace
  logs.
- The CLI is still mostly a snapshot renderer. It does not expose enough recent
  peer-event history to diagnose a peer churn episode from one screen.
- The durable observability model is still text-first. A structured event store
  is needed if we want to answer questions like "which peer disconnected us and
  why?" after the run exits.

## Immediate Next Step

Add structured observability around execution peer dialing and session lifecycle:

- count discovered, cached, primed, added, active, closed, and removed EL peers
- capture recent peer events in runtime telemetry
- surface the recent event ring in the CLI output
- preserve the existing text audit log for raw evidence
