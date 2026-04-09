# Mempool Oracle P2P Completion Tracker

## Objective

Finish `crates/mempooloracle` so `TrackerTransport::P2p(...)` is a single usable library transport that:

- gets pending transaction gossip and pool backfill from the execution-layer `reth` network stack
- gets block continuity from the consensus-layer `eth2_libp2p` stack
- preserves strict block continuity semantics for tracker state
- keeps the existing RPC transport working unchanged

From the caller's point of view, P2P is one thing. The execution and consensus network split is internal.

## Current Status

The transport and build baseline are in place and the direct P2P runtime now exists in a working first form.

What works now:

- transport-neutral tracker runtime/API exists
- RPC transport still works
- `reth` execution P2P is wired for pending tx intake and initial pool backfill
- `eth2_libp2p` is vendored intact and builds behind `consensus-p2p`
- the P2P transport now boots both network stacks together
- consensus beacon block gossip is subscribed and converted into `MempoolEvent::NewBlock`
- block gaps no longer silently continue; the tracker now has an explicit reset path

## Completed Work

1. Refactored `mempooloracle` to support transport-neutral runtime startup while preserving RPC helpers.
2. Added feature-gated `reth-p2p` and `consensus-p2p` dependency support.
3. Vendored the Grandine workspace intact so `eth2_libp2p` can build from this repository.
4. Removed the fake "disabled block transport" path from the public P2P configuration.
5. Added tracker reset support:
   - `MempoolEvent::Reset`
   - `TrackerReset`
   - `MempoolInner::reset(...)`
6. Added continuity-oriented telemetry fields for consensus block handling.
7. Replaced the old stubbed `transport/p2p.rs` path with a combined runtime that:
   - starts embedded `reth`
   - starts embedded `eth2_libp2p`
   - backfills the pool
   - ingests pending tx updates
   - subscribes to beacon block gossip
   - uses `BlocksByRange` for short-gap recovery
   - resets and re-anchors instead of keeping state across unrecovered gaps

## Remaining Work

1. Replace the current synthetic local consensus STATUS payload with a better one:
   - use a real head root once anchor state is known
   - advertise more accurate earliest available slot semantics
2. Improve recovery peer selection:
   - today recovery requests are sent to the peer that delivered the gap-triggering block
   - prefer peers with better status/head information when available
3. Harden recovery buffering:
   - keep and drain post-anchor buffered blocks after reset when safe
   - avoid unnecessary discard on recoverable reorderings
4. Add dedicated tests for the new semantics:
   - reset on unrecovered gap
   - successful contiguous recovery via `BlocksByRange`
   - anchor behind head with continued block-by-block processing
   - tx intake while recovery is in progress
5. Add a feature-gated smoke/integration test for the combined transport.
6. Expose the new consensus continuity telemetry in any downstream dashboards or CLI views that should display it.
7. Review whether the temporary consensus network directory should become configurable or cleaned up more explicitly.

## Execution Order

1. Keep the current combined transport compiling and passing core tests.
2. Add direct tests around reset and recovery behavior.
3. Improve consensus STATUS/head tracking.
4. Improve recovery peer selection and buffer handling.
5. Add smoke coverage for the full combined P2P mode.
6. Only after the runtime is operationally solid, consider cleanup or ergonomics work.

## Continuity Rules

These are the rules the runtime must keep obeying:

1. It may anchor from a block behind head.
2. Once anchored at block `N`, it must process `N+1`, `N+2`, and so on without skipping.
3. If a later block arrives before one or more required predecessor blocks:
   - try short-gap recovery first
   - if recovery fails, reset tracker state and re-anchor
4. It must never keep existing tracker state while silently skipping missing blocks.

## Acceptance Criteria

The P2P implementation is "good enough to use" when all of the following are true:

1. `cargo check -p mempooloracle --features reth-p2p,consensus-p2p` passes.
2. `cargo test -p mempooloracle` passes.
3. In P2P mode, pending tx gossip populates the oracle without RPC.
4. In P2P mode, consensus block gossip drives `NewBlock` updates without RPC.
5. A recoverable short block gap is filled contiguously.
6. An unrecoverable gap triggers reset-and-reanchor instead of silent continuation.
7. RPC mode still behaves as before.

## Open Risks

- The current local consensus STATUS message is minimal and may be too weak for some peer sets.
- Recovery currently assumes short recent gaps; very large or pathological gaps will reset.
- The first implementation favors correctness over retention, so some resets may be broader than strictly necessary.
- The combined runtime has not yet been proven by a dedicated end-to-end feature-gated integration test.
