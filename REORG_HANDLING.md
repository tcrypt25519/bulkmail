# Reorg Handling Architecture & Known Issues

This document outlines the current implementation of reorg handling in the `mempooloracle` crate, identifies critical logical flaws, and lists open architectural questions.

## Current Implementation

Reorgs are detected primarily by a **Parent Hash Mismatch**. When a block $N$ is received, if its `parent_hash` does not match the `hash` of our current block $N-1$, a reorg event is triggered.

The current handler performs a "walk-back":
1. It iterates backward through the local block history.
2. It stops when it finds a block whose hash matches the `parent_hash` of a block in the new fork (or specifically, the "trigger" block).
3. It detaches the abandoned fork, returning transactions to the pending pool.
4. It attaches the new block.

## Critical Logical Flaws

### 1. The Intermediate Gap Problem (Deep Reorgs)
If a reorg happens several blocks back (e.g., we were at height 100, and we see a new block at height 101 built on a parent from height 98), the walk-back correctly identifies the divergence at height 98. 
**The Flaw:** The current implementation attaches block 101 directly to block 98, effectively "skipping" the canonical blocks at heights 99 and 100 on the new fork. This creates a broken chain state where `block(101).parent != block(100)`.

### 2. Finality Violation
Ethereum (and other PoS chains) have a concept of **Finality**. A block that has been finalized should never be reverted.
**The Flaw:** The walk-back logic does not refer to the consensus state's finalized head. If a reorg is deep enough to attempt a walk-back past the finalized boundary, the system should treat this as a critical error (consensus failure) rather than continuing.

### 3. Canonical Chain Selection
Currently, the system is biased toward the "newest seen block" that indicates a reorg.
**The Flaw:** There is no weighting mechanism (like Ethereum's LMD GHOST) to determine which fork is actually canonical. If we receive a block for height $N+1$ that disagrees with our head at $N$, we assume the new chain is correct. In a high-latency network, this could lead to frequent, unnecessary oscillations between forks.

## Open Questions

1. **Pause Optimism?** Should we temporarily stop "optimistic" block tracking when a reorg is detected? 
   - *Pro:* Prevents calculating state based on a potentially invalid chain.
   - *Con:* Induces liveness failures and increased latency for mempool observation.
2. **How to Fetch the Gap?** When a deep reorg is detected, what is the most efficient mechanism to "backfill" the missing intermediate blocks on the new fork?
3. **Handling Finality Failures:** If a reorg attempts to revert a finalized block, how should the oracle recover? Is a full state reset required?
4. **Mempool Consistency:** How do we ensure that account nonces and balances remain consistent when switching forks, especially if the "new" blocks are not immediately available for all heights?

## Thoughts on Recovery

The core issue is that `handle_reorg` is currently a **reactive** and **atomic** operation, whereas a proper reorg recovery is a **state transition** that may require multiple network requests to fill the gaps.

We likely need a "Recovery Mode" for the `TrackerRuntime` where:
- Incoming "normal" blocks are buffered.
- The system focuses on fetching the contiguous chain from the common ancestor to the new head.
- Only once the chain is contiguous and verified does the system resume normal processing.
