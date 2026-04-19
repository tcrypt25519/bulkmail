# Mempool Oracle P2P Completion Tracker

## Objective

Finish `crates/mempooloracle` so `TrackerTransport::P2p(...)` is a single usable library transport that:

- gets pending transaction gossip and pool backfill from the execution-layer `reth` network stack
- gets block continuity and finality from the consensus-layer `eth2_libp2p` stack
- preserves strict block continuity semantics for tracker state
- keeps the existing RPC transport working unchanged

From the caller's point of view, P2P is one thing. The execution and consensus network split is internal.

## Current Status

The transport and build baseline are in place and the direct P2P runtime is fully operational and verified with live mainnet data.

What works now:
- transport-neutral tracker runtime/API exists
- RPC transport still works
- `reth` execution P2P is wired for pending tx intake and initial pool backfill
- `eth2_libp2p` is vendored intact and builds behind `consensus-p2p`
- the P2P transport boots both network stacks together
- consensus beacon block gossip is converted into `MempoolEvent::NewBlock`
- consensus finality update gossip is converted into `MempoolEvent::FinalizedBlock`
- Execution Layer status is dynamically updated from Consensus Layer gossip to maintain peer connectivity
- Comprehensive P2P instrumentation and persistent audit logging are implemented

## Completed Work

1.  Refactored `mempooloracle` to support transport-neutral runtime startup while preserving RPC helpers.
2.  Added feature-gated `reth-p2p` and `consensus-p2p` dependency support.
3.  Vendored the Grandine workspace intact so `eth2_libp2p` can build from this repository.
4.  Removed the fake "disabled block transport" path from the public P2P configuration.
5.  Added tracker reset support (`MempoolEvent::Reset`).
6.  Replaced the old stubbed `transport/p2p.rs` path with a combined runtime.
7.  **Block Finalization:** Added support for `LightClientFinalityUpdate` and `FinalizedBlock` events.
8.  **Dynamic Synchronization:** Implemented dynamic EL status updates (block number/hash) driven by CL block gossip to prevent peer disconnections.
9.  **Accurate Network Context:** Configured the system with current mainnet bootnodes, chainspec, and a reliable initial slot.
10. **Enhanced Instrumentation:** Added tracking for peer counts, message counts, latencies, and unfinalized depth.
11. **Audit Logging:** Implemented an optional persistent P2P event log for recording all connections and message flow.

## Remaining Work

1.  **Attach/Detach Semantics & Reorg Handling:**
    *   **Strict Continuity:** Implement `attach_block(N+1)` which requires block `N` as its predecessor. Returns an error if a gap is detected (N+2 or later).
    *   **Block History:** Maintain a circular buffer (ring buffer) of the last 32 attached blocks (number + hash).
    *   **Detach Logic:** Implement `detach_block(block)` which reverses block effects and identifies transactions to be restored to the pool.
    *   **Reorg Recovery:**
        - Detect fork when `parent_hash` of new block `N` != `hash` of our block `N-1`.
        - Walk back through the ring buffer to find the **Common Ancestor**.
        - Call `detach_block` on all blocks from current head back to the common ancestor.
        - Collect detached transactions into a "restore set".
        - Iterate forward on the new chain, calling `attach_block` for each block.
        - During attachment, re-remove any transactions that appear in the new blocks.
        - Update the real mempool with the final net-change set (additions from detached blocks, removals based on new account nonces).

2.  **Intelligent Multi-Peer Recovery:**
    *   **Chunking:** Implement pagination for `BlocksByRange` requests (protocol limit is typically 128).
    *   **Parallelism:** Send requests for different chunks to multiple peers simultaneously.
    *   **Retry Logic:** If a peer fails, retry with a different peer for that specific chunk.
    *   **Patience:** Only escalate to a full Prune/Re-anchor after multiple peer failures and a timeout.

3.  **Intelligent Pruning (Escalation Path):**
    *   **Nonce Tracking:** On a re-anchor, look ahead at all available/buffered blocks and build a map of latest nonces per account.
    *   **Selective Retention:** Keep transactions with nonces > latest seen in blocks; discard those with earlier nonces.
    *   **Aging & Base Fee Guards:** Keep old transactions (e.g. > 1hr) even if blocks were missed. Keep transactions paying < minimum base fee.
    *   **Full Mempool Sync:** On re-anchor, request a full mempool snapshot from multiple peers and merge/prune instead of clearing.

4.  **Transaction Metadata:**
    *   Add `seen_at: SystemTime` to `PendingTx` to support aging logic.

5.  **Network Configuration Cleanup:**
    *   Make consensus network directory and P2P ports (30303, 9000) configurable.

6.  **Automated Semantic Testing:**
    *   Tests for the precise reorg walk-back, contiguous recovery, and selective pruning.

## Execution Order

1.  **Core Semantics:** Implement `attach_block`/`detach_block` and the 32-block ring buffer.
2.  **Reorg Logic:** Implement the walk-back and restore-set recovery process.
3.  **Metadata & Aging:** Add transaction timestamps.
4.  **Parallel Recovery:** Implement chunked, multi-peer block fetching in `p2p.rs`.
5.  **Testing:** Add automated unit/integration tests for these semantic rules.

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
