# Mempool Oracle P2P Completion Tracker

## Objective

Finish `crates/mempooloracle` so `TrackerTransport::P2p(...)` is a single usable library transport that:

- gets pending transaction gossip and pool backfill from the execution-layer `reth` network stack
- gets block continuity and finality from the consensus-layer `eth2_libp2p` stack
- preserves strict block continuity semantics for tracker state
- keeps the existing RPC transport working unchanged

From the caller's point of view, P2P is one thing. The execution and consensus network split is internal.

## Current Status

The transport and build baseline are in place, and the runtime can boot both
network stacks with the feature set enabled. Consensus Layer live runs have
connected to peers and received block/finality traffic. The Execution Layer path
now performs discovery and seed-cache priming, but sustained EL sessions and
pending transaction gossip are still unresolved.

What works now:
- transport-neutral tracker runtime/API exists
- RPC transport still works
- `reth` execution P2P is wired for pending tx intake and initial pool backfill
- `eth2_libp2p` is vendored intact and builds behind `consensus-p2p`
- the P2P transport boots both network stacks together
- consensus beacon block gossip is converted into `MempoolEvent::NewBlock`
- consensus finality update gossip is converted into `MempoolEvent::FinalizedBlock`
- persistent P2P audit logging is implemented
- build issues have been resolved with feature gating for `reth-p2p`
- EL discovery and local seed-cache priming are implemented

## Completed Work

1.  Refactored `mempooloracle` to support transport-neutral runtime startup while preserving RPC helpers.
2.  Added feature-gated `reth-p2p` and `consensus-p2p` dependency support.
3.  Vendored the Grandine workspace intact so `eth2_libp2p` can build from this repository.
4.  Removed the fake "disabled block transport" path from the public P2P configuration.
5.  Added tracker reset support (`MempoolEvent::Reset`).
6.  Replaced the old stubbed `transport/p2p.rs` path with a combined runtime.
7.  Added support for `LightClientFinalityUpdate` and `FinalizedBlock` events.
8.  Configured the system with current mainnet bootnodes, chainspec, and runtime fork context.
9.  Added tracking for peer counts, message counts, latencies, and unfinalized depth.
10. Implemented an optional persistent P2P event log for recording connections and message flow.
11. Added EL discovery logging and local execution peer seed-cache priming.

## Remaining Work

1.  **Reorg Detection & Fork Handling:**
    *   Completed: Attach/detach semantics exist with `attach_block()` and `detach_block()`.
    *   Completed: A 32-block circular history is maintained in `MempoolInner.history`.
    *   Partial: Parent-hash validation exists, but fork walk-back needs live-path hardening and tests.

2.  **Gap Recovery System:**
    *   Completed: Multi-peer chunked recovery exists with 128-block chunks.
    *   Completed: Block buffering and ordered emission are implemented.
    *   Partial: Escalation to full reset when recovery fails needs refinement.

3.  **Intelligent Pruning (Escalation Path):**
    *   Not started: Nonce-based selective retention on re-anchor.
    *   Not started: Aging guards for old transactions.
    *   Not started: Base fee guards for low-fee transactions.
    *   Not started: Full mempool sync instead of wholesale clearing.

4.  **Transaction Metadata:**
    *   Completed: `seen_at: SystemTime` exists on `PendingTx`.

5.  **Network Configuration Cleanup:**
    *   Partial: Basic port configuration exists.
    *   Not started: Consensus network directory should be configurable.

6.  **Automated Semantic Testing:**
    *   Not started: Tests for reorg walk-back, contiguous recovery, and selective pruning.

7.  **Build System:**
    *   Completed: Feature gating and compilation fixes are implemented.

## Execution Order

Completed:
1.  Core semantics: `attach_block`/`detach_block` and the 32-block ring buffer.
2.  Metadata: transaction timestamps through `seen_at`.
3.  Parallel recovery: chunked, multi-peer block fetching in `p2p.rs`.

Remaining:
4.  Reorg logic: harden and test walk-back recovery for fork handling.
5.  Intelligent pruning: add nonce-based selective retention and aging guards.
6.  Configuration: make network directories and ports configurable.
7.  Testing: add automated unit/integration tests for semantic rules.
8.  EL observability: expose connection attempts, disconnect reasons, and recent peer-event history.

## Continuity Rules

1.  It may anchor from a block behind head.
2.  Once anchored at block `N`, it must process `N+1`, `N+2`, and so on without skipping.
3.  If a later block arrives before required predecessor blocks, trigger parallel chunked recovery.
4.  If a fork is detected, perform a **Walk-back Detach** to the common ancestor and re-apply the new branch.
5.  If recovery fails after exhaustive peer attempts, perform an **Intelligent Prune** and re-anchor.

## Acceptance Criteria

1.  **Attach/Detach:** Block processing is strictly ordered and reversible.
2.  **Reorg Resilience:** Fork events up to 32 blocks deep are handled by detaching and re-attaching without total state loss.
3.  **Gap Recovery:** Recoverable short block gaps are filled contiguously via multi-peer requests.
4.  **Selective Pruning:** Re-anchoring preserves "in-flight" transactions (based on nonces and aging).

## Open Risks

- **Pruning Complexity:** Intelligent pruning is significantly more complex than a wholesale reset and requires careful nonce/aging logic.
- **Ring Buffer Depth:** Reorgs deeper than 32 blocks will still require a full re-anchor/prune.
- **Sync Drift:** EL/CL drift may cause temporary status mismatches during high network volatility.

## Summary & Next Steps

Overall completion: roughly 70%.

The build and combined P2P runtime are in place, and the Consensus Layer path
has live evidence. The highest-priority runtime gap is Execution Layer peer
stability and transaction gossip. Reorg handling, intelligent pruning, and
configuration cleanup remain production-readiness items.
