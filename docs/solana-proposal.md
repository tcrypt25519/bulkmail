# Proposal: Adding Solana Support to Bulkmail

## 1. Executive Summary

This proposal describes how to extend Bulkmail — currently an Ethereum-specific
parallel transaction sender — to also support Solana while sharing as much
infrastructure as possible. The design introduces a chain-agnostic core that
retains the existing priority queue, concurrency management, retry logic, and
deadline system, while allowing chain-specific modules to handle the fundamental
differences in transaction construction, fee pricing, sequencing, and
confirmation between Ethereum and Solana.

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
| `chain.rs` | `ChainClient` trait + Alloy WebSocket implementation |
| `lib.rs` | Public API, error types |

The system already has strong architectural seams:

1. **`ChainClient` trait** — `Sender` depends on `Arc<dyn ChainClient>`, not a
   concrete type, enabling mocking and alternative implementations.
2. **Late binding** — Nonces and gas prices are assigned immediately before
   sending, not at message creation.
3. **Separation of intent from execution** — `Message` carries only the caller's
   intent; chain-specific fields are bound by the managers.

These seams are precisely where Solana support can be inserted.

---

## 3. Solana Transaction Model — Key Differences

Understanding the fundamental differences between Ethereum and Solana is
critical before designing the abstraction layer.

### 3.1 No Global Account Nonce

Ethereum uses a per-account monotonic nonce to order transactions and prevent
replays. Solana uses a **recent blockhash** (valid for ~150 slots / ~60 seconds)
embedded in each transaction. There is no sequential ordering constraint — all
transactions from the same account are independent.

**Implication:** The `NonceManager` concept does not apply to Solana. Instead,
Solana requires a **blockhash manager** that fetches and caches recent
blockhashes, refreshing them before they expire.

Solana also offers **durable nonce accounts** for long-lived transactions, but
these are not suitable for high-throughput sending (each nonce account supports
only one pending transaction). For Bulkmail's use case — high-frequency
real-time sending — recent blockhashes are the correct choice.

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
2. **Trait-based polymorphism at the chain boundary.** Generalize `ChainClient`
   and the fee/sequencing managers into traits that both Ethereum and Solana
   implement.
3. **Feature-gated dependencies.** Use Cargo features (`ethereum`, `solana`)
   so users only pull in the dependencies they need.
4. **No compromise on either chain's capabilities.** The abstraction should not
   prevent chain-specific optimizations (e.g., Solana's parallel execution,
   Ethereum's nonce replacement).

### 4.2 Module Reorganization

```
src/
├── lib.rs                    # Public API, Error enum, feature gates
├── message.rs                # Chain-agnostic Message (unchanged core)
├── priority_queue.rs         # Chain-agnostic (unchanged)
├── sender.rs                 # Chain-agnostic orchestrator (parameterized)
│
├── chain/
│   ├── mod.rs                # ChainClient trait (generalized)
│   ├── ethereum.rs           # Ethereum ChainClient (current chain.rs)
│   └── solana.rs             # Solana ChainClient (new)
│
├── fee/
│   ├── mod.rs                # FeeManager trait
│   ├── ethereum.rs           # EIP-1559 GasPriceManager (current gas_price.rs)
│   └── solana.rs             # Compute-unit priority fee manager (new)
│
├── sequencing/
│   ├── mod.rs                # SequenceManager trait
│   ├── ethereum.rs           # NonceManager (current nonce_manager.rs)
│   └── solana.rs             # BlockhashManager (new)
│
└── bin/
    ├── oracle.rs             # Ethereum oracle (current)
    └── oracle_solana.rs      # Solana oracle (new, uses solana-test-validator)
```

### 4.3 Core Trait Definitions

#### 4.3.1 Generalized ChainClient

The current `ChainClient` trait is Ethereum-specific (returns `TxEip1559`,
`PendingTransactionBuilder<Ethereum>`, etc.). We generalize it:

```rust
/// Chain-agnostic interface for submitting transactions and
/// monitoring blocks/slots.
#[async_trait]
pub trait ChainClient: Send + Sync {
    /// A chain-specific unique identifier (chain ID or cluster name).
    fn chain_id(&self) -> String;

    /// Subscribe to new block/slot notifications.
    async fn subscribe_new_blocks(&self) -> Result<BlockReceiver, Error>;

    /// Get the current block/slot number.
    async fn get_block_number(&self) -> Result<u64, Error>;

    /// Send a fully-constructed, signed transaction and return
    /// its identifier for tracking confirmation.
    async fn send_transaction(
        &self,
        tx: SignedTransaction,
    ) -> Result<TransactionId, Error>;

    /// Check the confirmation status of a previously sent transaction.
    async fn get_transaction_status(
        &self,
        id: TransactionId,
    ) -> Result<TransactionStatus, Error>;
}
```

Where `SignedTransaction`, `TransactionId`, and `TransactionStatus` are enums
dispatching to chain-specific types:

```rust
pub enum SignedTransaction {
    Ethereum(alloy::consensus::TxEnvelope),
    Solana(solana_sdk::transaction::Transaction),
}

pub enum TransactionId {
    Ethereum(alloy::primitives::B256),
    Solana(solana_sdk::signature::Signature),
}

pub enum TransactionStatus {
    Pending,
    Confirmed { slot_or_block: u64 },
    Finalized { slot_or_block: u64 },
    Failed { reason: String },
    Expired,
}
```

**Alternative (generic) approach:** Instead of enums, the trait could be made
generic over associated types. This would be more type-safe but less
ergonomic for dynamic dispatch. Given that Bulkmail already uses
`Arc<dyn ChainClient>`, the enum approach is recommended for consistency and
simplicity.

#### 4.3.2 FeeManager Trait

```rust
/// Chain-agnostic fee computation.
#[async_trait]
pub trait FeeManager: Send + Sync {
    /// Compute fee parameters for a transaction with the given
    /// message priority (0–100).
    ///
    /// Returns an opaque `FeeParams` that the chain client knows how to apply.
    async fn get_fee_params(&self, priority: u32) -> Result<FeeParams, Error>;

    /// Update internal estimates based on a confirmed transaction.
    async fn update_on_confirmation(
        &self,
        confirmation_time: Duration,
        fee_paid: FeeParams,
    );

    /// Return the current base fee estimate (for fee bumping).
    async fn get_base_fee(&self) -> FeeParams;
}
```

Where `FeeParams` is:

```rust
pub enum FeeParams {
    /// EIP-1559: (base_fee_per_gas, max_priority_fee_per_gas)
    Ethereum { base_fee: u128, priority_fee: u128 },
    /// Solana: compute_unit_price in micro-lamports
    Solana { compute_unit_price: u64, compute_unit_limit: u32 },
}
```

The Ethereum `FeeManager` implementation is the existing `GasPriceManager`
wrapped to produce `FeeParams::Ethereum`. The Solana implementation queries
`getRecentPrioritizationFees` and adapts using the same rolling-window
congestion analysis pattern.

#### 4.3.3 SequenceManager Trait

```rust
/// Chain-agnostic transaction sequencing / replay protection.
#[async_trait]
pub trait SequenceManager: Send + Sync {
    /// Acquire sequencing state for a new transaction.
    /// On Ethereum: returns the next nonce.
    /// On Solana: returns a recent blockhash (may require an RPC call).
    async fn acquire(&self) -> Result<SequenceToken, Error>;

    /// Release sequencing state for a transaction that was not sent.
    /// On Ethereum: marks the nonce available for reuse.
    /// On Solana: no-op (blockhashes are not exclusive).
    async fn release(&self, token: SequenceToken);

    /// Notify the manager that a transaction has confirmed.
    /// On Ethereum: advances the nonce baseline.
    /// On Solana: no-op.
    async fn confirm(&self, token: SequenceToken);

    /// Sync with the chain (called on each new block/slot).
    async fn sync(&self) -> Result<(), Error>;
}

pub enum SequenceToken {
    Nonce(u64),
    Blockhash(solana_sdk::hash::Hash),
}
```

The Ethereum implementation is the existing `NonceManager`. The Solana
implementation caches the latest blockhash and refreshes it on each slot
notification or when it approaches expiry (~150 slots).

### 4.4 Sender Changes

The `Sender` is parameterized over the three traits rather than depending on
concrete types. The core loop remains identical:

```rust
pub struct Sender {
    chain: Arc<dyn ChainClient>,
    sequencer: Arc<dyn SequenceManager>,
    fee_manager: Arc<dyn FeeManager>,
    // ... queue, pending, semaphore, message_ready (unchanged)
}
```

The key change is in `send_transaction`: instead of building a `TxEip1559`
directly, it delegates to a **transaction builder** function that the chain
client provides (or that is selected based on the chain type).

#### Retry Logic Adaptation

The retry flow must diverge based on the chain:

| Scenario | Ethereum | Solana |
|----------|----------|--------|
| Transaction stuck | Replace at same nonce with +20% fee | Resubmit fresh transaction with new blockhash and higher priority fee |
| Transaction dropped | Detected via timeout, replace | Detected via blockhash expiry or status polling; resubmit |
| Max retries | Abandon message | Abandon message |
| Fee bumping | `new_fee = fee + fee * 20%` | `new_cu_price = cu_price + cu_price * 20%` |

The existing `handle_transaction_dropped` flow is modified to check the chain
type:

- **Ethereum path** (unchanged): resubmit at the same nonce with a bumped fee.
- **Solana path**: acquire a fresh blockhash, rebuild and re-sign the
  transaction with a bumped compute unit price, and submit as a new
  transaction.

The `PendingTransaction` struct gains a `SequenceToken` field in place of the
`nonce: u64` field, and the replacement logic branches accordingly.

### 4.5 Message Changes

The `Message` struct remains chain-agnostic. However, the chain-specific data
it carries must be flexible:

```rust
pub struct Message {
    // Shared fields (unchanged)
    pub priority: u32,
    pub deadline: Option<DateTime<Utc>>,
    created_at: Instant,
    retry_count: u32,

    // Chain-specific transaction payload
    pub payload: TransactionPayload,
}

pub enum TransactionPayload {
    Ethereum {
        to: Option<Address>,
        value: U256,
        data: Bytes,
        gas: u64,
    },
    Solana {
        instructions: Vec<solana_sdk::instruction::Instruction>,
        /// Compute unit limit for this transaction (default: 200,000).
        compute_units: u32,
    },
}
```

The `effective_priority` calculation is unchanged — it depends only on
`priority`, `retry_count`, `created_at`, and `deadline`, none of which are
chain-specific.

### 4.6 Solana ChainClient Implementation

```rust
pub struct SolanaChain {
    rpc_client: Arc<RpcClient>,
    pubsub_client: Arc<PubsubClient>,
    keypair: Arc<Keypair>,
    commitment: CommitmentConfig,
}
```

**Dependencies** (feature-gated under `solana`):

| Crate | Purpose |
|-------|---------|
| `solana-sdk` | `Keypair`, `Transaction`, `Instruction`, `Hash`, `Signature` |
| `solana-client` | `RpcClient` (HTTP/HTTPS), `PubsubClient` (WebSocket) |
| `solana-transaction-status` | Transaction status types |

**Key methods:**

- `subscribe_new_blocks`: Uses `PubsubClient::slot_subscribe` to receive slot
  notifications, bridged into an mpsc channel (same pattern as the Ethereum
  implementation).
- `send_transaction`: Builds a `solana_sdk::transaction::Transaction` from the
  message's instructions, prepends `ComputeBudgetProgram` instructions for
  priority fee and compute limit, signs with the `Keypair`, and sends via
  `RpcClient::send_transaction` with `skip_preflight: true` (preflight is
  done separately in simulation).
- `get_transaction_status`: Uses `RpcClient::get_signature_statuses` to poll
  for confirmation at the configured commitment level.

### 4.7 Solana FeeManager Implementation

```rust
pub struct SolanaFeeManager {
    /// Rolling window of confirmation times (same pattern as GasPriceManager).
    confirmation_times: Mutex<VecDeque<Duration>>,
    /// Current compute unit price estimate (micro-lamports).
    compute_unit_price: Mutex<u64>,
}
```

**Fee estimation flow:**

1. Periodically call `getRecentPrioritizationFees` (targeting the accounts the
   messages will touch).
2. Compute the average/median recent fee.
3. Apply the same congestion-level multiplier pattern (Low/Medium/High based on
   confirmation times).
4. Apply the message priority multiplier (100%–200%).
5. Clamp to a configurable maximum.

This mirrors the `GasPriceManager` design exactly, just with different units
(micro-lamports per compute unit instead of wei per gas).

### 4.8 Solana SequenceManager Implementation

```rust
pub struct SolanaBlockhashManager {
    rpc_client: Arc<RpcClient>,
    /// Cached recent blockhash and the slot at which it was fetched.
    state: Mutex<BlockhashState>,
}

struct BlockhashState {
    blockhash: Hash,
    last_valid_block_height: u64,
    fetched_at: Instant,
}
```

**Behavior:**

- `acquire()` returns the cached blockhash. If the cached blockhash is older
  than ~45 seconds (leaving ~15s safety margin before the ~60s expiry, based on
  ~150 slots at 400ms), it fetches a fresh one first. Returns
  `Result<SequenceToken, Error>` so that RPC failures (network errors, node
  unavailability) are properly propagated to the caller.
- `release()` is a no-op (blockhashes are not exclusive).
- `confirm()` is a no-op (no nonce to advance).
- `sync()` is called each slot; refreshes the blockhash if approaching expiry.

Unlike Ethereum's `NonceManager`, there is no contention — multiple concurrent
transactions can share the same recent blockhash without conflict. This
simplifies the design considerably.

---

## 5. Feature Gating and Dependencies

### 5.1 Cargo.toml Changes

```toml
[features]
default = ["ethereum"]
ethereum = ["alloy", "secp256k1", "ethereum-types"]
solana = ["solana-sdk", "solana-client", "solana-transaction-status"]

[dependencies]
# Shared
tokio = { version = "1.49", features = ["full"] }
async-trait = "0.1"
chrono = "0.4"
log = "0.4"
thiserror = "2.0"
# ... other shared deps

# Ethereum (feature-gated)
alloy = { version = "1.7", optional = true, ... }
secp256k1 = { version = "0.31", optional = true }
ethereum-types = { version = "0.16", optional = true }

# Solana (feature-gated)
solana-sdk = { version = "2.2", optional = true }
solana-client = { version = "2.2", optional = true }
solana-transaction-status = { version = "2.2", optional = true }
```

This ensures that a project using Bulkmail for Ethereum-only does not pull in
the Solana SDK (which is substantial), and vice versa.

### 5.2 Conditional Compilation

Chain-specific modules use `#[cfg(feature = "ethereum")]` and
`#[cfg(feature = "solana")]` gates. The core modules (`message.rs`,
`priority_queue.rs`, `sender.rs`) are always compiled but use the trait
abstractions.

The `TransactionPayload` and `FeeParams` enums' variants are also
feature-gated:

```rust
pub enum TransactionPayload {
    #[cfg(feature = "ethereum")]
    Ethereum { ... },
    #[cfg(feature = "solana")]
    Solana { ... },
}
```

**Note:** At least one of the `ethereum` or `solana` features must be enabled;
otherwise `TransactionPayload` (and similar enums like `FeeParams`,
`SequenceToken`) would have no variants, causing a compilation error. The
`default = ["ethereum"]` feature ensures this for the common case. When both
features are enabled simultaneously (e.g., an application that bridges between
chains), all variants are present and the `Sender` can be instantiated with
either chain's implementations. A compile-time assertion should be added to
emit a clear error message if neither feature is enabled:

```rust
#[cfg(not(any(feature = "ethereum", feature = "solana")))]
compile_error!("At least one chain feature (\"ethereum\" or \"solana\") must be enabled.");
```

---

## 6. Solana-Specific Considerations

### 6.1 Transaction Confirmation Polling

Solana does not have Ethereum's `PendingTransactionBuilder` watcher. Instead,
confirmation must be polled:

```rust
async fn watch_transaction(
    &self,
    signature: Signature,
    timeout: Duration,
) -> Result<TransactionStatus, Error> {
    let deadline = Instant::now() + timeout;
    loop {
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
                    slot_or_block: status.slot,
                });
            }
        }

        if Instant::now() >= deadline {
            return Ok(TransactionStatus::Expired);
        }

        tokio::time::sleep(Duration::from_millis(400)).await;
    }
}
```

The 400ms polling interval is appropriate for Solana's ~400ms slot time.

### 6.2 Transaction Timeout

On Ethereum, `TX_TIMEOUT = 3s` is used. On Solana, transactions are naturally
bounded by blockhash expiry (~60 seconds). However, we want faster feedback.
A reasonable Solana timeout is **15–30 seconds** — if a transaction hasn't
confirmed by then, it's likely not going to be picked up by the current leader
and should be retried with a fresh blockhash.

This timeout should be chain-configurable rather than a compile-time constant.

### 6.3 Retry Without Replacement

Since Solana has no nonce-based replacement, retries on Solana create entirely
new transactions:

1. Fetch fresh blockhash.
2. Rebuild transaction with bumped compute unit price.
3. Re-sign with keypair.
4. Submit as a new, independent transaction.

The old transaction may still confirm (it's valid until its blockhash expires).
To handle this, the `Sender` should track both the old and new transaction
signatures and consider the message confirmed if **either** lands. This is a
minor addition to the pending map — instead of a single `TxHash`, the Solana
path maintains a `Vec<Signature>` per message.

### 6.4 Preflight Simulation

Solana supports transaction simulation via `simulateTransaction`. This should
be used before sending to catch errors early (insufficient balance, program
errors, compute budget exceeded). The existing `SimulationFailed` error variant
already exists in the `Error` enum and can be reused.

### 6.5 Concurrency

Solana's parallel execution model means the semaphore cap can potentially be
higher than Ethereum's 16. Since there are no nonce-ordering constraints,
Solana can sustain more concurrent in-flight transactions. Consider making
`MAX_IN_FLIGHT_TRANSACTIONS` a configurable parameter with a higher default
for Solana (e.g., 64).

---

## 7. Component Mapping Summary

| Bulkmail Concept | Ethereum Implementation | Solana Implementation |
|------------------|------------------------|----------------------|
| **Chain Client** | Alloy WebSocket, EthereumWallet, secp256k1 | `RpcClient` + `PubsubClient`, `Keypair`, Ed25519 |
| **Block Subscription** | `eth_subscribe("newHeads")` via Alloy | `slotSubscribe` via `PubsubClient` |
| **Sequencing** | `NonceManager` (monotonic nonce tracking) | `BlockhashManager` (recent blockhash caching) |
| **Fee Management** | `GasPriceManager` (EIP-1559 base + priority fee) | `SolanaFeeManager` (compute unit price) |
| **Fee Data Source** | Confirmation time EMA | `getRecentPrioritizationFees` + confirmation time EMA |
| **Transaction Building** | `TxEip1559` struct | `solana_sdk::transaction::Transaction` with `ComputeBudget` instructions |
| **Signing** | `EthereumWallet` (secp256k1) | `Keypair::sign_message` (Ed25519) |
| **Confirmation** | Alloy's `PendingTransactionBuilder` with timeout | Polling `get_signature_statuses` with commitment level |
| **Stuck Tx Handling** | Same nonce, bumped fee (replacement) | Fresh blockhash, bumped CU price (new transaction) |
| **Timeout** | 3 seconds (fast EVM chain) | 15–30 seconds (configurable) |
| **Max Concurrency** | 16 | 64 (configurable; no ordering constraint) |
| **Simulation** | Not yet implemented | `simulateTransaction` before send |

---

## 8. Implementation Plan

### Phase 1: Trait Extraction (Non-Breaking)

**Goal:** Extract the current Ethereum-specific code behind the new traits
without changing external behavior.

1. Define `FeeManager`, `SequenceManager` traits and the `FeeParams`,
   `SequenceToken` types.
2. Implement `FeeManager` for the existing `GasPriceManager`.
3. Implement `SequenceManager` for the existing `NonceManager`.
4. Refactor `Sender` to depend on `Arc<dyn FeeManager>` and
   `Arc<dyn SequenceManager>` instead of the concrete types.
5. Generalize the `ChainClient` trait (keep the existing Ethereum-specific
   methods behind a feature gate while adding the generic interface).
6. Introduce `TransactionPayload` on `Message` in a backward-compatible way:
   - Add a new `payload: Option<TransactionPayload>` field alongside the
     existing public `to`, `value`, `data`, and `gas` fields.
   - Update internal code paths to prefer `payload` when it is `Some`, while
     continuing to support the existing fields so external callers are not
     broken.
   - Mark the old fields as `#[deprecated]` in the documentation and plan
     their removal in a subsequent 0.2.x release.
7. All existing tests pass without modification.

**Estimated effort:** 2–3 days.

### Phase 2: Solana Implementation

**Goal:** Implement Solana-specific modules behind the `solana` feature flag.

1. Add `solana-sdk`, `solana-client` dependencies (feature-gated).
2. Implement `SolanaChain` (`ChainClient` for Solana).
3. Implement `SolanaFeeManager` (`FeeManager` for Solana).
4. Implement `SolanaBlockhashManager` (`SequenceManager` for Solana).
5. Implement Solana-specific retry logic in `Sender` (fresh blockhash +
   bumped CU price; multi-signature pending tracking).
6. Add unit tests with mock RPC responses.

**Estimated effort:** 4–5 days.

### Phase 3: Oracle Simulation & Integration Testing

**Goal:** Validate end-to-end with a local Solana test validator.

1. Create `oracle_solana.rs` using `solana-test-validator`.
2. Test concurrent SOL transfers (analogous to the Anvil oracle).
3. Test retry and fee-bumping scenarios.
4. Test blockhash expiry handling.
5. Performance benchmarking (throughput comparison).

**Estimated effort:** 2–3 days.

### Phase 4: Documentation & Polish

1. Update `ARCHITECTURE.md` with multi-chain design.
2. Update `README.md` with Solana usage examples.
3. Add `docs/solana.md` with Solana-specific details.
4. Add `CHANGELOG.md` entry.

**Estimated effort:** 1 day.

---

## 9. Risk Analysis

| Risk | Mitigation |
|------|-----------|
| **Solana SDK crate size** — `solana-sdk` pulls in many transitive dependencies | Feature gating ensures Ethereum-only users are unaffected |
| **Blockhash expiry race** — Transaction built with a near-expiry blockhash | `BlockhashManager` enforces a ~15-second safety margin; retries use fresh blockhashes |
| **Duplicate confirmations** — Both old and retried Solana transactions confirm | Track all signatures per message; accept first confirmation, ignore subsequent ones |
| **Account contention** — Many transactions writing to the same account are serialized | Configurable concurrency cap; callers should batch instructions per-account |
| **RPC rate limits** — High polling frequency for confirmations | Exponential backoff; configurable polling interval; batch `get_signature_statuses` calls |
| **API stability** — Solana SDK versions change frequently | Pin to a stable 2.x release; abstract behind trait so SDK upgrades are localized |

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
| Retry counting (`MAX_RETRIES`) | Chain-independent retry budget |
| Error types (most) | `MessageExpired`, `RetriesExceeded`, etc. are generic |

This represents roughly **60% of the codebase** remaining identical, which
validates the claim that the current architecture is well-suited for
multi-chain extension.

---

## 11. Conclusion

Bulkmail's existing architecture — trait-based chain abstraction, late-bound
transaction parameters, separation of message intent from execution details —
provides an excellent foundation for Solana support. The key insight is that
while Ethereum and Solana differ fundamentally in sequencing (nonces vs.
blockhashes), fee structure (EIP-1559 vs. compute units), and retry strategy
(replacement vs. resubmission), these differences can be cleanly encapsulated
behind three traits (`ChainClient`, `FeeManager`, `SequenceManager`) while
sharing the orchestration, priority, concurrency, and retry-budget logic.

The recommended Rust crates for the Solana implementation are:

| Crate | Version | Purpose |
|-------|---------|---------|
| `solana-sdk` | 2.2.x | Key management (Ed25519 `Keypair`), transaction construction, `Instruction`, `Hash`, `Signature` |
| `solana-client` | 2.2.x | `RpcClient` for JSON-RPC, `PubsubClient` for WebSocket slot subscriptions |
| `solana-transaction-status` | 2.2.x | Transaction status types and commitment level handling |

The estimated total effort is **9–12 days** across four phases, with Phase 1
(trait extraction) being backward-compatible and independently shippable.
The existing public `Message` fields are preserved alongside the new
`TransactionPayload` during Phase 1, with deprecation and removal deferred
to a subsequent 0.2.x release.
