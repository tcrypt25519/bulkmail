# Proposal: Adding Solana Support to Bulkmail

## 1. Executive Summary

This proposal describes how to extend Bulkmail — currently an Ethereum-specific
parallel transaction sender — to also support Solana while sharing as much
infrastructure as possible. The design introduces a **generic adapter pattern**
with chain-specific tag types, so that `Sender` is monomorphized per chain at
compile time. The priority queue, concurrency management, retry budgets, and
deadline system are chain-agnostic; chain-specific concerns (transaction
building, fee pricing, replay protection, retry mechanics, and confirmation
watching) are encapsulated behind associated types on a single `ChainAdapter`
trait.

The initial implementation uses enum-based dispatch internally for expedience,
but the trait boundaries are designed so that generic associated types (GATs)
can replace dynamic dispatch without any public API changes.

---

## 2. Current Architecture Recap

Bulkmail's data flow today:

```
Message → PriorityQueue → Sender.run() loop → NonceManager → GasPriceManager → Chain → Ethereum
```

Key modules and their responsibilities:

| Module | Responsibility |
|--------|---------------|
| `message.rs` | Transaction intent, effective priority calculation |
| `priority_queue.rs` | Max-heap ordering by effective priority |
| `sender.rs` | Orchestrator: concurrency, retry, fee bumping |
| `nonce_manager.rs` | Ethereum nonce tracking and synchronization |
| `gas_price.rs` | EIP-1559 priority fee adaptation |
| `chain.rs` | Concrete `Chain` struct wrapping Alloy WebSocket provider |
| `lib.rs` | Public API, error types |
| `bin/oracle.rs` | Simulation binary using Anvil (to be moved to `examples/`) |

The existing `ChainClient` trait is the primary extension point — `Sender`
already depends on `Arc<dyn ChainClient>`, not a concrete type. `NonceManager`
and `GasPriceManager` are currently concrete types; this proposal promotes them
to trait boundaries alongside `ChainClient`.

---

## 3. Solana Transaction Model — Key Differences

Understanding the fundamental differences between Ethereum and Solana is
critical before designing the abstraction layer.

### 3.1 No Global Account Nonce

Ethereum uses a per-account monotonic nonce to order transactions and prevent
replays. Solana uses a **recent blockhash** (valid for ~150 blocks by block
height / ~60 seconds) embedded in each transaction. There is no sequential
ordering constraint — all transactions from the same account are independent.

Slot count and block height diverge because empty/skipped slots advance wall
time but not block height; `lastValidBlockHeight` from the RPC is the
authoritative expiry measure.

**Implication:** The `NonceManager` concept does not apply to Solana. Instead,
Solana requires a **blockhash manager** that maintains a cache of recent
blockhashes in memory, refreshing proactively so that transaction crafting
never requires an RPC round-trip. The role is better described as **replay
protection** rather than sequencing, since Solana imposes no ordering.

Solana also offers **durable nonce accounts** for long-lived transactions, but
these are not suitable for high-throughput sending: transactions are strictly
sequential per nonce account (each must advance the nonce before the next can
use it). For Bulkmail's use case — high-frequency real-time sending — recent
blockhashes are the correct choice.

### 3.2 No Mempool Replacement

Ethereum allows replacing a pending transaction by resubmitting at the same
nonce with a higher fee. Solana has **no public mempool** and **no replacement
mechanism**. Transactions are streamed directly to the current leader validator
via Gulf Stream.

**Implication:** The `bump_transaction_fee` / replacement flow does not apply.
On Solana, the retry strategy is: **resubmit a fresh transaction** with a new
blockhash and potentially higher priority fee, rather than replacing an
existing one.

### 3.3 Fee Model: Compute Units + Priority Fee

Ethereum uses EIP-1559 with `base_fee` and `priority_fee` (per gas unit).
Solana uses:

- **Base fee**: Fixed at 5,000 lamports per signature (non-negotiable).
- **Priority fee**: `compute_unit_price` (micro-lamports per compute unit) ×
  `compute_unit_limit`. Set via `ComputeBudgetProgram` instructions prepended
  to the transaction.

**Implication:** The `GasPriceManager` must be abstracted. The Solana
implementation will manage `compute_unit_price` rather than EIP-1559 fields,
using `getRecentPrioritizationFees` RPC data for adaptation.

### 3.4 Parallel Execution and Account Locking

Solana transactions must declare all accounts they read/write upfront.
Transactions touching disjoint accounts execute in parallel; conflicting
transactions are serialized or one is dropped.

**Implication:** This is actually favorable for Bulkmail — it already caps
concurrency via a semaphore. Solana's parallel execution means Bulkmail can
achieve even higher throughput than on Ethereum without nonce-ordering
constraints.

### 3.5 Transaction Size Limits

Solana transactions are limited to ~1,232 bytes (one MTU packet). This is much
smaller than Ethereum's gas-limited transactions.

**Implication:** The `Message` struct must accommodate this constraint. For
Solana, each message carries a vector of `Instruction` values rather than
opaque `Bytes` calldata, and the builder must validate that the final
serialized transaction fits within the packet limit.

### 3.6 Confirmation Model

Ethereum has a single confirmation depth model. Solana has three commitment
levels:

| Level | Meaning |
|-------|---------|
| `processed` | Transaction seen by the connected node |
| `confirmed` | Voted on by a supermajority of stake |
| `finalized` | Irreversible; rooted in the ledger |

**Implication:** Bulkmail should let callers choose the commitment level. For
high-throughput sending to a fast chain, `confirmed` is the natural default
(analogous to 1 confirmation on Ethereum).

### 3.7 Signing: Ed25519 vs secp256k1

Ethereum uses secp256k1 ECDSA. Solana uses Ed25519 (via `Keypair` from
`solana-sdk`).

**Implication:** Key management must be chain-specific. The `Chain`
implementation already encapsulates signing, so this is a natural boundary.

---

## 4. Proposed Architecture

### 4.1 Design Principles

1. **Shared core, chain-specific edges.** The priority queue, concurrency
   management (semaphore), retry/deadline logic, and orchestrator loop are
   chain-agnostic and should be shared.
2. **Monomorphized adapter pattern.** A single `ChainAdapter` trait with
   associated types groups all chain-specific components together. `Sender<A>`
   is generic over the adapter, enabling zero-cost dispatch and type-safe
   binding of all chain components via a tag type.
3. **Feature-gated dependencies.** Use Cargo features (`ethereum`, `solana`)
   so users only pull in the dependencies they need.
4. **No compromise on either chain's capabilities.** The abstraction should not
   prevent chain-specific optimizations (e.g., Solana's parallel execution,
   Ethereum's nonce replacement).
5. **No RPC at transaction-packaging time.** Replay protection tokens (nonces,
   blockhashes) and fee estimates must be available from in-memory state. RPC
   calls happen asynchronously in background refresh loops, never in the hot
   path of crafting a transaction.

### 4.2 Module Reorganization

```
src/
├── lib.rs                        # Public API, Error enum, feature gates
├── message.rs                    # Chain-agnostic Message (unchanged core)
├── priority_queue.rs             # Chain-agnostic (unchanged)
├── sender.rs                     # Sender<A: ChainAdapter> orchestrator
│
├── adapter/
│   ├── mod.rs                    # ChainAdapter trait + associated types
│   ├── ethereum.rs               # adapters::Eth tag type + all ETH impls
│   └── solana.rs                 # adapters::Sol tag type + all SOL impls
│
examples/
├── oracle_eth.rs                 # Ethereum oracle (moved from bin/oracle.rs)
└── oracle_sol.rs                 # Solana oracle (new, uses solana-test-validator)
```

The existing `chain.rs`, `gas_price.rs`, and `nonce_manager.rs` modules are
absorbed into `adapter/ethereum.rs`. Their public interfaces are preserved as
methods on the Ethereum adapter's associated types. The `bin/oracle.rs`
simulation is moved to `examples/oracle_eth.rs` (it has always been an example
in practice). A Solana equivalent is added as `examples/oracle_sol.rs`.

### 4.3 The ChainAdapter Trait

Rather than three separate traits (`ChainClient`, `FeeManager`,
`SequenceManager`) that must be manually kept in sync, a single `ChainAdapter`
trait groups them as associated types:

```rust
/// The root abstraction that binds all chain-specific components together.
///
/// Each chain (Ethereum, Solana) provides a zero-sized tag type that
/// implements this trait and specifies its associated types. `Sender<A>`
/// is generic over this trait, so all chain components are statically
/// bound and monomorphized at compile time.
pub trait ChainAdapter: Send + Sync + 'static {
    /// Chain-specific transaction payload (e.g., EIP-1559 fields or Solana
    /// instructions).
    type Payload: Send + Sync + Clone;

    /// Chain-specific fee parameters.
    type FeeParams: Send + Sync + Clone;

    /// Chain-specific replay-protection token (nonce or blockhash).
    type ReplayToken: Send + Sync + Clone;

    /// Opaque transaction identifier returned after sending.
    type TxId: Send + Sync + Clone;

    /// The chain connection / RPC client.
    type Client: ChainClient<Self> + Send + Sync;

    /// Fee estimation and adaptation.
    type FeeManager: FeeManager<Self> + Send + Sync;

    /// Replay protection (nonce management or blockhash caching).
    type ReplayProtection: ReplayProtection<Self> + Send + Sync;

    /// Transaction retry / fee-bump strategy.
    type RetryStrategy: RetryStrategy<Self> + Send + Sync;
}
```

Pre-built adapters:

```rust
pub mod adapters {
    /// Ethereum adapter tag. All ETH-specific types are associated here.
    pub struct Eth;

    /// Solana adapter tag. All SOL-specific types are associated here.
    pub struct Sol;
}
```

Usage:

```rust
// Ethereum
let sender: Sender<adapters::Eth> = Sender::new(eth_client, eth_fees, eth_nonces, eth_retry);

// Solana
let sender: Sender<adapters::Sol> = Sender::new(sol_client, sol_fees, sol_blockhash, sol_retry);
```

The tag type carries no data — it exists only to lock all associated types
together at the type level. This prevents accidentally mixing an Ethereum fee
manager with a Solana client.

### 4.4 Component Traits

Each associated type on `ChainAdapter` implements a component trait. These
traits use the adapter's associated types rather than enums, so there is no
size overhead from mismatched variants.

#### 4.4.1 ChainClient

```rust
#[async_trait]
pub trait ChainClient<A: ChainAdapter>: Send + Sync {
    /// Subscribe to new block/slot notifications.
    async fn subscribe_new_blocks(&self) -> Result<BlockReceiver, Error>;

    /// Get the current block number (ETH) or confirmed block height (SOL).
    async fn get_block_number(&self) -> Result<u64, Error>;

    /// Build, sign, and submit a transaction.
    async fn send_transaction(
        &self,
        payload: &A::Payload,
        fee: A::FeeParams,
        replay_token: A::ReplayToken,
    ) -> Result<A::TxId, Error>;

    /// Check confirmation status of a previously sent transaction.
    async fn get_transaction_status(
        &self,
        id: &A::TxId,
    ) -> Result<TransactionStatus, Error>;
}
```

`TransactionStatus` is chain-agnostic and uses only primitive types:

```rust
pub enum TransactionStatus {
    Pending,
    /// On Ethereum: the block number. On Solana: the slot number in which
    /// the transaction was confirmed (which is the chronological measure
    /// used by `get_signature_statuses`).
    Confirmed { number: u64 },
    Finalized { number: u64 },
    Failed { reason: String },
    Expired,
}
```

#### 4.4.2 FeeManager

```rust
#[async_trait]
pub trait FeeManager<A: ChainAdapter>: Send + Sync {
    /// Compute fee parameters for a given message priority (0–100).
    async fn get_fee_params(&self, priority: u32) -> Result<A::FeeParams, Error>;

    /// Record a confirmed transaction's latency and fee for adaptation.
    async fn update_on_confirmation(
        &self,
        confirmation_time: Duration,
        fee_paid: &A::FeeParams,
    );

    /// Bump fee params for a retry (e.g., +20%).
    fn bump_fee(&self, current: &A::FeeParams) -> A::FeeParams;
}
```

#### 4.4.3 ReplayProtection

```rust
#[async_trait]
pub trait ReplayProtection<A: ChainAdapter>: Send + Sync {
    /// Get a replay-protection token for a new transaction.
    /// Must be serviced from in-memory state — no RPC calls.
    fn next(&self) -> A::ReplayToken;

    /// Sync with the chain (called on each new block/slot notification).
    /// This is where RPC calls to refresh state happen.
    async fn sync(&self) -> Result<(), Error>;
}
```

Note the key differences from the previous `SequenceManager` design:

- **Renamed** from "SequenceManager" to "ReplayProtection" because Solana
  replay protection is explicitly not sequencing.
- **`next()` is synchronous** — it reads from in-memory state only. Blockhash
  acquisition is fully decoupled from transaction crafting. The `sync()` method
  handles all RPC I/O in the background.
- **No `release()` method** in the shared trait. On Ethereum, the nonce pool
  is managed internally by the `EthReplayProtection` implementation; when a
  transaction fails before broadcast, the implementation's own `release_nonce()`
  method is called directly by the Ethereum retry strategy — not through the
  generic trait.

Ethereum implementation:

```rust
/// Wraps the existing NonceManager. next() is infallible and in-memory.
pub struct EthReplayProtection {
    nonce_manager: NonceManager,
}

impl ReplayProtection<Eth> for EthReplayProtection {
    fn next(&self) -> u64 {
        self.nonce_manager.get_next_available_nonce()
    }
    async fn sync(&self) -> Result<(), Error> {
        self.nonce_manager.sync_nonce().await
    }
}

impl EthReplayProtection {
    /// Ethereum-only: return a nonce to the available pool.
    pub fn release_nonce(&self, nonce: u64) { ... }

    /// Ethereum-only: advance the confirmed baseline.
    pub fn confirm_nonce(&self, nonce: u64) { ... }
}
```

Solana implementation:

```rust
/// Maintains a cache of recent blockhashes, refreshed proactively by sync().
pub struct SolReplayProtection {
    /// Most recent blockhash and its expiry block height.
    state: Mutex<BlockhashState>,
}

struct BlockhashState {
    blockhash: [u8; 32],
    last_valid_block_height: u64,
    /// Slot number at which this blockhash was fetched.
    fetched_at_slot: u64,
}

impl ReplayProtection<Sol> for SolReplayProtection {
    fn next(&self) -> [u8; 32] {
        self.state.lock().blockhash
    }
    async fn sync(&self) -> Result<(), Error> {
        // Fetch current block height; if remaining validity < half the
        // window (~75 blocks), proactively refresh. The comparison is
        // between current block height and `last_valid_block_height`,
        // both of which are chain-reported values.
        ...
    }
}
```

The half-window refresh threshold (~75 blocks out of the ~150 block validity
window) ensures we never use a blockhash that's close to expiring, while also
not over-fetching. The determination is based on the difference between the
current block height and `last_valid_block_height` — both chain-reported
values, not wall-clock time.

#### 4.4.4 RetryStrategy

```rust
#[async_trait]
pub trait RetryStrategy<A: ChainAdapter>: Send + Sync {
    /// Handle a transaction that was dropped or timed out.
    ///
    /// The strategy decides whether to bump fees and resubmit,
    /// re-queue the message, or abandon it.
    async fn handle_dropped(
        &self,
        pending: &PendingTransaction<A>,
        client: &A::Client,
        fees: &A::FeeManager,
        replay: &A::ReplayProtection,
    ) -> RetryDecision<A>;

    /// Handle a confirmed transaction.
    async fn handle_confirmed(
        &self,
        pending: &PendingTransaction<A>,
        fees: &A::FeeManager,
        replay: &A::ReplayProtection,
        confirmation_time: Duration,
    );
}

pub enum RetryDecision<A: ChainAdapter> {
    /// Resubmit with new fee params and replay token.
    Resubmit {
        fee: A::FeeParams,
        replay_token: A::ReplayToken,
    },
    /// Re-queue the message for a fresh attempt later.
    Requeue,
    /// Abandon the message.
    Abandon,
}
```

This moves all retry logic out of `Sender` and into the adapter. The Ethereum
strategy resubmits at the same nonce with bumped fees; the Solana strategy
acquires a fresh blockhash and bumps the compute unit price. Both strategies
share the same budgeting counters (`replacement_count`, `retry_count`).

This also future-proofs the design: if an alternative Ethereum nonce strategy
(e.g., the proposed 256-parallel-nonce scheme) gains traction, it can be
implemented as a second Ethereum adapter without modifying `Sender` or the
existing Ethereum adapter.

### 4.5 Sender Changes

`Sender` becomes generic over the adapter:

```rust
pub struct Sender<A: ChainAdapter> {
    client: Arc<A::Client>,
    fees: Arc<A::FeeManager>,
    replay: Arc<A::ReplayProtection>,
    retry: Arc<A::RetryStrategy>,

    queue: SharedQueue,
    pending: SharedPendingMap<A>,
    max_in_flight: Arc<Semaphore>,
    message_ready: Arc<Notify>,
}
```

The core `run()` loop, `add_message()`, `process_next_message()`, and
semaphore management are identical across chains. Chain-specific behavior is
fully delegated to the adapter's component implementations:

- **Transaction building and sending** → `A::Client::send_transaction()`
- **Fee computation** → `A::FeeManager::get_fee_params()`
- **Replay protection** → `A::ReplayProtection::next()`
- **Retry/drop handling** → `A::RetryStrategy::handle_dropped()`
- **Confirmation feedback** → `A::RetryStrategy::handle_confirmed()`

### 4.6 Message Changes

During Phase 1, the `Message` struct gains a `payload` field alongside the
existing `to`, `value`, `data`, and `gas` fields for backward compatibility:

```rust
pub struct Message {
    // Existing fields (preserved during Phase 1 for backward compatibility)
    pub to: Option<Address>,
    pub value: U256,
    pub data: Bytes,
    pub gas: u64,

    // Shared fields
    pub priority: u32,
    pub deadline: Option<DateTime<Utc>>,
    created_at: Instant,
    retry_count: u32,

    // Chain-specific payload (added in Phase 1)
    pub payload: Option<TransactionPayload>,
}
```

Internal code paths prefer `payload` when it is `Some`, falling back to the
existing fields otherwise. The old fields are marked `#[deprecated]` and
removed in a subsequent 0.2.x release, at which point `payload` becomes
required (non-`Option`).

The final target struct (post-0.2.x) is:

```rust
pub struct Message {
    pub priority: u32,
    pub deadline: Option<DateTime<Utc>>,
    created_at: Instant,
    retry_count: u32,
    pub payload: TransactionPayload,
}
```

`TransactionPayload` variants are feature-gated:

```rust
pub enum TransactionPayload {
    #[cfg(feature = "ethereum")]
    Ethereum {
        to: Option<Address>,
        value: U256,
        data: Bytes,
        gas: u64,
    },
    #[cfg(feature = "solana")]
    Solana {
        instructions: Vec<solana_sdk::instruction::Instruction>,
        compute_units: u32,
    },
}
```

The `effective_priority` calculation is unchanged — it depends only on
`priority`, `retry_count`, `created_at`, and `deadline`, none of which are
chain-specific.

### 4.7 Solana ChainClient Implementation

```rust
pub struct SolanaChain {
    rpc_client: Arc<RpcClient>,
    pubsub_client: Arc<PubsubClient>,
    keypair: Arc<Keypair>,
    commitment: CommitmentConfig,
}
```

**Dependencies** (feature-gated under `solana`):

| Crate | Purpose | Feature flags |
|-------|---------|---------------|
| `solana-sdk` | `Keypair`, `VersionedTransaction`, `Instruction`, `Hash`, `Signature` | `default-features = false` — disable unused features aggressively |
| `solana-client` | `RpcClient` (HTTP/HTTPS), `PubsubClient` (WebSocket) | `default-features = false` |
| `solana-transaction-status` | Transaction status types | `default-features = false` |

All Solana crates are added with `default-features = false` and only the
specific features needed are enabled, to minimize the transitive dependency
footprint.

**Key methods:**

- `subscribe_new_blocks`: Uses `PubsubClient::slot_subscribe` to receive slot
  notifications, bridged into an mpsc channel (same pattern as the Ethereum
  implementation).
- `send_transaction`: Receives a `TransactionPayload`, `FeeParams`, and a
  blockhash; builds a `VersionedTransaction` from the message's instructions
  (prepending `ComputeBudgetProgram` instructions for priority fee and compute
  unit limit), signs with the stored `Keypair`, and sends via
  `RpcClient::send_transaction` with `skip_preflight: true`.
  `VersionedTransaction` is used in preference to the legacy `Transaction` type
  because it supports Address Lookup Tables, which are required when
  approaching the 1,232-byte packet limit.
- `get_transaction_status`: Uses `RpcClient::get_signature_statuses` to check
  confirmation at the configured commitment level.

### 4.8 Solana FeeManager Implementation

```rust
pub struct SolanaFeeManager {
    confirmation_times: Mutex<VecDeque<Duration>>,
    compute_unit_price: Mutex<u64>,
}
```

**Fee estimation flow:**

1. Periodically call `getRecentPrioritizationFees` (targeting the accounts the
   messages will touch).
2. Compute the 50/50 moving average of recent fees (same algorithm as
   `GasPriceManager`).
3. Apply the same congestion-level multiplier pattern (Low/Medium/High based on
   confirmation times).
4. Apply the message priority multiplier (100%–200%).
5. Clamp to a configurable maximum.

This mirrors the `GasPriceManager` design exactly, just with different units
(micro-lamports per compute unit instead of wei per gas).

### 4.9 Solana ReplayProtection Implementation

```rust
pub struct SolReplayProtection {
    state: Mutex<BlockhashState>,
}

struct BlockhashState {
    blockhash: [u8; 32],
    last_valid_block_height: u64,
    /// The slot number at which this blockhash was fetched, used to
    /// determine age for refresh decisions.
    fetched_at_slot: u64,
}
```

**Behavior:**

- `next()` is synchronous — returns the cached blockhash from in-memory state.
  No RPC call is made. This ensures transaction crafting never blocks on I/O.
- `sync()` is called on each slot notification. It compares the current block
  height to `last_valid_block_height`. If the remaining validity is less than
  half the window (~75 blocks out of ~150), it proactively refreshes the
  cached blockhash via `getLatestBlockhash`. The half-window threshold is
  measured from the slot time of the last-fetched blockhash, not from
  wall-clock time.

Multiple concurrent transactions share the same blockhash without conflict.

### 4.10 Solana Confirmation Watching

Solana does not have Ethereum's `PendingTransactionBuilder` watcher.
Confirmation is driven by **slot subscription notifications**, not by
polling with sleep intervals:

```rust
async fn watch_transaction(
    &self,
    id: &SolTxId,
    slot_receiver: &mut SlotReceiver,
) -> Result<TransactionStatus, Error> {
    loop {
        // Wait for the next slot notification — no sleep, no polling.
        match slot_receiver.recv().await {
            Some(slot_info) => {
                let statuses = self.rpc_client
                    .get_signature_statuses(&[signature])
                    .await?;
                if let Some(Some(status)) = statuses.value.first() {
                    if status.err.is_some() {
                        return Ok(TransactionStatus::Failed {
                            reason: format!("{:?}", status.err),
                        });
                    }
                    if status.satisfies_commitment(self.commitment) {
                        return Ok(TransactionStatus::Confirmed {
                            number: status.slot,
                        });
                    }
                }
                // Check if the blockhash has expired by block height.
                let block_height = self.rpc_client
                    .get_block_height().await?;
                if block_height > last_valid_block_height {
                    return Ok(TransactionStatus::Expired);
                }
            }
            None => {
                return Err(Error::SubscriptionClosed);
            }
        }
    }
}
```

This approach:
- **Never sleeps** — confirmation checks are driven by slot subscription
  events, eliminating non-deterministic alignment issues.
- **Checks on every slot** — more responsive than sleeping for a fixed
  interval.
- **Uses block height for expiry** — not wall-clock time. The chain's own
  block height is the only reliable source of truth for blockhash validity.

**Note:** If multiple transactions are watched concurrently, consider batching
signature status checks. A centralized watcher can collect signatures from
all in-flight transactions and call `getSignatureStatuses` once per slot,
then fan out results to reduce RPC load.

### 4.11 Transaction Timeout

On Ethereum, `TX_TIMEOUT = 3s` is used. On Solana, transactions are naturally
bounded by blockhash expiry (~150 blocks / ~60 seconds). The Solana timeout is
driven by **block height**, not wall-clock time — the chain's own progression
determines when a transaction is too old, which is more reliable than any
local timer.

The `RetryStrategy` implementation uses the `last_valid_block_height` from the
blockhash to determine when a transaction has expired. This is inherently
chain-accurate, unlike clock-based timeouts that can drift.

### 4.12 Retry Strategy Divergence

| Scenario | Ethereum | Solana |
|----------|----------|--------|
| Transaction stuck | Replace at same nonce with +20% fee | Resubmit with new blockhash + higher CU price |
| Transaction dropped | Detected via timeout, replace | Detected via block-height expiry; resubmit |
| Max replacements | Re-queue message | Re-queue message |
| Max retries | Abandon message | Abandon message |
| Fee bumping | `new_fee = fee + fee * 20%` | `new_cu_price = cu_price + cu_price * 20%` |

Both strategies share the two-tier budget: `MAX_REPLACEMENTS` caps per-
transaction fee bumps; `MAX_RETRIES` caps per-message retries end-to-end.

When a Solana transaction is retried, the old transaction may still confirm
(it's valid until its blockhash expires). The retry strategy tracks all
outstanding transaction IDs per message and considers the message confirmed if
**any** of them lands, ignoring subsequent confirmations.

---

## 5. Feature Gating and Dependencies

### 5.1 Cargo.toml Changes

```toml
[features]
default = ["ethereum"]
ethereum = ["dep:alloy", "dep:secp256k1", "dep:ethereum-types"]
solana = ["dep:solana-sdk", "dep:solana-client", "dep:solana-transaction-status"]

[dependencies]
# Shared
tokio = { version = "1.49", features = ["full"] }
async-trait = "0.1"
chrono = "0.4"
log = "0.4"
thiserror = "2.0"

# Ethereum (feature-gated)
alloy = { version = "1.7", optional = true, default-features = false, features = [
    "std", "provider-ws", "pubsub", "rpc-types", "signer-local", "k256"
] }
secp256k1 = { version = "0.31", optional = true }
ethereum-types = { version = "0.16", optional = true }

# Solana (feature-gated, aggressively trimmed)
solana-sdk = { version = "2.2", optional = true, default-features = false }
solana-client = { version = "2.2", optional = true, default-features = false }
solana-transaction-status = { version = "2.2", optional = true, default-features = false }
```

### 5.2 Conditional Compilation

```rust
#[cfg(not(any(feature = "ethereum", feature = "solana")))]
compile_error!("At least one chain feature (\"ethereum\" or \"solana\") must be enabled.");
```

Chain-specific adapter modules use `#[cfg(feature = "...")]` gates. The core
modules (`message.rs`, `priority_queue.rs`, `sender.rs`) are always compiled.

---

## 6. Solana-Specific Considerations

### 6.1 Preflight Simulation

Both Ethereum and Solana support transaction simulation, but neither chain has
simulation logic implemented in Bulkmail today. The `SimulationFailed` variant
in the `Error` enum is a placeholder — it is never constructed anywhere in the
codebase. Full simulation wiring (`simulateTransaction` call, error
extraction, integration into the send path) would need to be written from
scratch for either chain. This is out of scope for the initial Solana
implementation.

### 6.2 Concurrency

Solana's parallel execution model means the semaphore cap can potentially be
higher than Ethereum's 16. Since there are no nonce-ordering constraints,
Solana can sustain more concurrent in-flight transactions.
`MAX_IN_FLIGHT_TRANSACTIONS` should be a configurable parameter with a higher
default for Solana (e.g., 64).

---

## 7. Component Mapping Summary

| Bulkmail Concept | Ethereum Implementation | Solana Implementation |
|------------------|------------------------|----------------------|
| **Adapter Tag** | `adapters::Eth` | `adapters::Sol` |
| **Chain Client** | Alloy WebSocket, EthereumWallet, secp256k1 | `RpcClient` + `PubsubClient`, `Keypair`, Ed25519 |
| **Block Subscription** | `eth_subscribe("newHeads")` via Alloy | `slotSubscribe` via `PubsubClient` |
| **Replay Protection** | `EthReplayProtection` (nonce pool) | `SolReplayProtection` (blockhash cache) |
| **Fee Management** | `EthFeeManager` (EIP-1559 base + priority fee) | `SolFeeManager` (compute unit price) |
| **Retry Strategy** | `EthRetryStrategy` (same nonce, bumped fee) | `SolRetryStrategy` (new blockhash, bumped CU price) |
| **Transaction Building** | `TxEip1559` struct | `VersionedTransaction` with `ComputeBudget` instructions |
| **Signing** | `EthereumWallet` (secp256k1) | `Keypair::sign_message` (Ed25519) |
| **Confirmation** | `PendingTransactionBuilder` with timeout | Slot-subscription-driven `get_signature_statuses` |
| **Timeout** | 3 seconds (fast EVM chain) | Block-height-driven (chain-accurate) |
| **Max Concurrency** | 16 | 64 (configurable) |
| **Simulation** | Not implemented | Not implemented (Phase 2+) |

---

## 8. Implementation Plan

### Phase 1: Trait Extraction and Adapter Pattern

**Goal:** Introduce the `ChainAdapter` trait and refactor the Ethereum code
behind it without breaking existing tests.

**Concrete steps:**

1. **Create `src/adapter/mod.rs`**: Define `ChainAdapter`, `ChainClient<A>`,
   `FeeManager<A>`, `ReplayProtection<A>`, `RetryStrategy<A>` traits and
   `RetryDecision<A>` type. Also define `TransactionStatus` at the crate
   root level (not as an adapter associated type) — it is chain-agnostic
   and uses only primitive types.

2. **Create `src/adapter/ethereum.rs`**: Define the `Eth` tag type and
   implement all associated types:
   - `impl ChainAdapter for Eth` — binds `Payload`, `FeeParams`,
     `ReplayToken`, `TxId`, and all component types.
   - `EthClient` — wraps the existing `Chain` struct, implementing
     `ChainClient<Eth>`.
   - `EthFeeManager` — wraps the existing `GasPriceManager`, implementing
     `FeeManager<Eth>`.
   - `EthReplayProtection` — wraps the existing `NonceManager`, implementing
     `ReplayProtection<Eth>` with a synchronous `next()` and an
     Ethereum-only `release_nonce()` / `confirm_nonce()`.
   - `EthRetryStrategy` — extracts the current `handle_transaction_dropped`
     and `bump_transaction_fee` logic from `sender.rs`, implementing
     `RetryStrategy<Eth>`.

3. **Refactor `src/sender.rs`**: Make `Sender` generic over `A: ChainAdapter`.
   Replace direct calls to `NonceManager`, `GasPriceManager`, and `Chain`
   with calls through the adapter's component traits. The `run()` loop
   structure, `add_message()`, `process_next_message()`, and semaphore
   management remain unchanged.

4. **Update `src/message.rs`**: Add `payload: Option<TransactionPayload>`
   alongside existing fields. Internal code paths prefer `payload` when
   `Some`. Mark old fields `#[deprecated]`.

5. **Update `src/lib.rs`**: Add `pub mod adapter;`, re-export adapter types,
   add feature-gate `compile_error!`.

6. **Move `src/bin/oracle.rs` → `examples/oracle_eth.rs`**: Update
   `Cargo.toml` to use `[[example]]` instead of `[[bin]]`.

7. **All existing tests pass** without modification (the Ethereum adapter
   is a direct wrapper around existing code).

**Estimated effort:** 3–4 days.

### Phase 2: Solana Implementation

**Goal:** Implement Solana-specific adapter behind the `solana` feature flag.

1. **Add Solana dependencies** to `Cargo.toml` (feature-gated, with
   `default-features = false`).

2. **Create `src/adapter/solana.rs`**: Define the `Sol` tag type and
   implement all associated types:
   - `SolClient` implementing `ChainClient<Sol>` — wraps `RpcClient` and
     `PubsubClient`.
   - `SolFeeManager` implementing `FeeManager<Sol>` — uses
     `getRecentPrioritizationFees`.
   - `SolReplayProtection` implementing `ReplayProtection<Sol>` — maintains
     blockhash cache, synchronous `next()`, block-height-driven `sync()`.
   - `SolRetryStrategy` implementing `RetryStrategy<Sol>` — fresh blockhash +
     bumped CU price; tracks multiple tx IDs per message.

3. **Add Solana-specific confirmation watching** driven by slot subscriptions.

4. **Unit tests** with mock RPC responses (same pattern as existing
   `MockChainClient` tests).

**Estimated effort:** 4–5 days.

### Phase 3: Examples & Integration Testing

**Goal:** Validate end-to-end with local test infrastructure.

1. **Create `examples/oracle_sol.rs`** using `solana-test-validator`.
2. Test concurrent SOL transfers (analogous to the Anvil oracle).
3. Test retry and fee-bumping scenarios.
4. Test blockhash expiry handling.
5. Performance benchmarking (throughput comparison).

**Estimated effort:** 2–3 days.

### Phase 4: Documentation & Polish

1. Update `README.md` with multi-chain usage examples.
2. Add `docs/solana.md` with Solana-specific operational details.
3. Add `CHANGELOG.md` entry.

**Estimated effort:** 1 day.

---

## 9. Risk Analysis

| Risk | Mitigation |
|------|-----------|
| **Solana SDK crate size** | Feature gating + `default-features = false` on all Solana crates |
| **Blockhash expiry race** | Half-window refresh threshold (~75 blocks); retries use fresh blockhashes |
| **Duplicate confirmations** | Track all tx IDs per message; accept first, ignore subsequent |
| **Account contention** | Configurable concurrency cap; callers batch instructions per-account |
| **RPC rate limits** | Exponential backoff; batch `get_signature_statuses`; slot-driven not timer-driven |
| **API stability** | Pin to stable 2.x release; abstract behind trait so SDK upgrades are localized |
| **Enum overhead** | Initial enum-based dispatch is temporary; adapter pattern is designed for direct monomorphization via GATs |

---

## 10. What Stays Shared

The following components are fully chain-agnostic and require zero changes:

| Component | Reason |
|-----------|--------|
| `PriorityQueue` | Operates on `Message` priority scores, independent of chain |
| `effective_priority()` | Depends only on priority, retries, age, deadline |
| Semaphore concurrency control | Generic concurrency limiting |
| `Sender::run` loop structure | `select!` on block notifications + message processing |
| `Sender::add_message` | Enqueue + notify |
| Deadline system | Wall-clock based, chain-independent |
| Retry budgets (`MAX_RETRIES` + `MAX_REPLACEMENTS`) | Chain-independent; strategies implement the budgeting logic |
| Error types (most) | `MessageExpired`, `RetriesExceeded`, etc. are generic |

This represents roughly **60% of the codebase** remaining identical.

---

## 11. Migration Path: Enums → Generic Associated Types

The initial implementation uses enums (`TransactionPayload`, potentially
`FeeParams`) for expedience. The adapter trait is designed to enable a clean
migration to fully monomorphized generics:

1. **Phase 1:** `Sender<A: ChainAdapter>` is generic. Associated types
   (`A::Payload`, `A::FeeParams`, `A::ReplayToken`, `A::TxId`) are concrete
   types specific to each adapter. Internally, some code may still use enum
   dispatch where convenient.

2. **Phase 2 (post-Solana):** Replace any remaining enum dispatch with direct
   associated type usage. Remove `Arc<dyn ...>` in favor of concrete types
   where possible. This eliminates the current `Arc<dyn ChainClient>` pattern
   that incurs virtual dispatch overhead.

3. **Final state:** `Sender<adapters::Eth>` and `Sender<adapters::Sol>` are
   fully monomorphized. Each chain's `FeeParams`, `ReplayToken`, and `TxId`
   are their natural sizes with no enum padding overhead (e.g., Solana's
   `FeeParams` is exactly 12 bytes, not padded to match Ethereum's 32 bytes).

---

## 12. Conclusion

Bulkmail's existing architecture — trait-based chain abstraction, late-bound
transaction parameters, separation of message intent from execution details —
provides an excellent foundation for Solana support. The key insight is that
while Ethereum and Solana differ fundamentally in replay protection (nonces vs.
blockhashes), fee structure (EIP-1559 vs. compute units), and retry strategy
(replacement vs. resubmission), these differences can be cleanly encapsulated
behind a single `ChainAdapter` trait with associated types, while sharing the
orchestration, priority, concurrency, and retry-budget logic.

The adapter pattern — with tag types (`adapters::Eth`, `adapters::Sol`)
binding all chain-specific components at the type level — ensures type safety,
eliminates the possibility of mismatched components, and provides a clear path
from initial enum-based dispatch to fully monomorphized generics.

The recommended Rust crates for the Solana implementation are:

| Crate | Version | Purpose | Features |
|-------|---------|---------|----------|
| `solana-sdk` | 2.2.x | Key management, tx construction, `Instruction`, `Hash`, `Signature` | `default-features = false` |
| `solana-client` | 2.2.x | `RpcClient` for JSON-RPC, `PubsubClient` for WebSocket | `default-features = false` |
| `solana-transaction-status` | 2.2.x | Transaction status types and commitment levels | `default-features = false` |

The estimated total effort is **10–13 days** across four phases.
