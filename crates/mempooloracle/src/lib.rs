//! A tracker for EIP-1559 mempool state and transaction confirmation latency.
mod runtime;
mod transport;

use alloy::providers::fillers::TxFiller;
use std::{
    cmp::Reverse,
    collections::{BTreeMap, HashMap, VecDeque},
    sync::{Arc, RwLock, mpsc::Receiver},
    time::SystemTime,
};

pub use runtime::{
    TrackerRuntime, TrackerTelemetry, TrackerTelemetrySnapshot, TransportKind, init_metrics,
};
#[cfg(feature = "reth-p2p")]
use transport::p2p;
use transport::rpc;

pub type AlloyTrackerRuntime = TrackerRuntime;
pub type AlloyTrackerTelemetry = TrackerTelemetry;
pub type AlloyTrackerTelemetrySnapshot = TrackerTelemetrySnapshot;

#[derive(Clone, Debug)]
pub enum TrackerTransport {
    Rpc(RpcTransportConfig),
    P2p(P2pTransportConfig),
}

#[derive(Clone, Debug)]
pub struct RpcTransportConfig {
    pub ws: alloy::providers::WsConnect,
}

#[derive(Clone, Debug)]
pub struct P2pTransportConfig {
    pub chain: String,
    pub bootnodes: Vec<String>,
    pub discovery_v4: bool,
    pub listen_addr: Option<std::net::SocketAddr>,
    pub execution_port: Option<u16>,
    pub consensus_port: Option<u16>,
    pub block_transport: P2pBlockTransport,
    pub log_path: Option<std::path::PathBuf>,
}

#[derive(Clone, Debug)]
pub enum P2pBlockTransport {
    ExecutionPolling,
    Consensus(ConsensusTransportConfig),
}

#[derive(Clone, Debug)]
pub struct ConsensusTransportConfig {
    pub implementation: ConsensusTransportImplementation,
}

#[derive(Clone, Debug)]
pub enum ConsensusTransportImplementation {
    Eth2Libp2p,
}

#[derive(Debug, thiserror::Error)]
pub enum TrackerError {
    #[error("alloy transport error: {0}")]
    Transport(#[from] alloy::transports::TransportError),
    #[error("timed out during {stage}")]
    Timeout { stage: &'static str },
    #[error("transport setup failed: {0}")]
    Setup(String),
    #[error("feature `{0}` is not enabled")]
    FeatureDisabled(&'static str),
    #[error("transport unsupported: {0}")]
    UnsupportedTransport(&'static str),
}

pub type AlloyTrackerError = TrackerError;

/// A Consensus Layer slot number.
#[derive(
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Debug,
    Default,
    serde::Serialize,
    serde::Deserialize,
)]
pub struct Slot(pub u64);

impl std::fmt::Display for Slot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// An Execution Layer block number.
#[derive(
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Debug,
    Default,
    serde::Serialize,
    serde::Deserialize,
)]
pub struct BlockNumber(pub u64);

impl std::fmt::Display for BlockNumber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::ops::Add<u64> for BlockNumber {
    type Output = Self;
    fn add(self, rhs: u64) -> Self::Output {
        Self(self.0 + rhs)
    }
}

impl std::ops::Sub<u64> for BlockNumber {
    type Output = Self;
    fn sub(self, rhs: u64) -> Self::Output {
        Self(self.0.saturating_sub(rhs))
    }
}

/// A Consensus Layer block or state root.
#[derive(
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Debug,
    Default,
    serde::Serialize,
    serde::Deserialize,
)]
pub struct BeaconHash(pub [u8; 32]);

/// An Execution Layer block or transaction hash.
#[derive(
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Debug,
    Default,
    serde::Serialize,
    serde::Deserialize,
)]
pub struct ExecutionHash(pub [u8; 32]);

impl From<[u8; 32]> for ExecutionHash {
    fn from(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl From<alloy::primitives::FixedBytes<32>> for ExecutionHash {
    fn from(bytes: alloy::primitives::FixedBytes<32>) -> Self {
        Self(bytes.0)
    }
}

/// A unique identifier for a transaction.
pub type TxId = ExecutionHash;

/// A unique identifier for an account.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Address(pub [u8; 20]);

/// An event that drives the mempool tracker.
#[derive(Debug, Clone)]
pub enum MempoolEvent {
    /// A new transaction has entered the mempool.
    PendingTransaction(PendingTx),
    /// The tracker must discard some state and restart from a fresh anchor.
    Prune(TrackerPrune),
    /// A new block has been mined.
    NewBlock(BlockUpdate),
    /// A block has been finalized.
    FinalizedBlock(Slot),
}

/// A prune and re-anchor instruction for the tracker.
#[derive(Debug, Clone)]
pub struct TrackerPrune {
    /// The fresh block to anchor to.
    pub anchor: BlockUpdate,
    /// Any blocks that were already seen but were ahead of the previous head.
    pub future_blocks: Vec<BlockUpdate>,
}

/// A pending transaction in the mempool.
#[derive(Debug, Clone)]
pub struct PendingTx {
    pub id: TxId,
    pub sender: Address,
    pub nonce: u64,
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
    pub gas_limit: u64,
    pub seen_at: SystemTime,
}

/// An update about a new block.
#[derive(Debug, Clone)]
pub struct BlockUpdate {
    /// The block number.
    pub number: BlockNumber,
    /// The block hash.
    pub hash: ExecutionHash,
    /// The parent block hash.
    pub parent_hash: ExecutionHash,
    /// The transactions included in the block.
    pub included_txs: Vec<PendingTx>,
    /// The new base fee for the next block.
    pub new_base_fee: u128,
    /// The total gas used by the block.
    pub gas_used: u64,
    /// The gas limit of the block.
    pub gas_limit: u64,
}

#[derive(Debug)]
struct AccountQueue {
    confirmed_nonce: u64,
    slots: Vec<Option<PendingTx>>,
}

pub struct MempoolInner {
    account_queues: HashMap<Address, AccountQueue>,
    base_fee_eligibility: BTreeMap<(u128, Address, u64), ()>,
    priority_queue: BTreeMap<(Reverse<u128>, Address, u64), ()>,
    config: TrackerConfig,
    current_base_fee: u128,
    private_flow_ratio: f64,
    last_block_gas_limit: u64,
    last_block_number: Option<BlockNumber>,
    last_block_hash: Option<ExecutionHash>,
    history: VecDeque<HistoryEntry>,
}

struct HistoryEntry {
    block: BlockUpdate,
    removed_txs: Vec<PendingTx>,
    prev_base_fee: u128,
    prev_private_flow_ratio: f64,
    prev_last_block_gas_limit: u64,
    prev_confirmed_nonces: HashMap<Address, u64>,
}

/// Configuration for the mempool tracker.
#[derive(Clone, Debug)]
pub struct TrackerConfig {
    /// The initial base fee to use before any blocks are received.
    pub initial_base_fee: u128,
    /// The maximum number of transactions to track in the mempool.
    pub global_capacity: usize,
    /// The maximum number of pending transactions to track per account.
    pub per_account_capacity: usize,
    /// The initial estimate for the private flow ratio.
    pub private_flow_prior: f64,
}

/// The mempool tracker, which runs in its own thread.
pub struct MempoolTracker {
    inner: Arc<RwLock<MempoolInner>>,
    rx: Receiver<MempoolEvent>,
}

impl MempoolTracker {
    /// Creates a tracker runtime from a selected transport.
    pub async fn connect(
        transport: TrackerTransport,
        config: TrackerConfig,
    ) -> Result<TrackerRuntime, TrackerError> {
        match transport {
            TrackerTransport::Rpc(rpc_config) => rpc::connect_with_config(rpc_config, config).await,
            TrackerTransport::P2p(_p2p_config) => {
                #[cfg(feature = "reth-p2p")]
                {
                    p2p::connect_with_config(_p2p_config, config).await
                }
                #[cfg(not(feature = "reth-p2p"))]
                {
                    Err(TrackerError::FeatureDisabled("reth-p2p"))
                }
            }
        }
    }

    /// Creates a new event-driven mempool tracker and its handle from a channel.
    pub fn from_channel(
        rx: Receiver<MempoolEvent>,
        initial_txs: &[PendingTx],
        config: TrackerConfig,
    ) -> (MempoolHandle, MempoolTracker) {
        let mut inner = MempoolInner {
            account_queues: HashMap::new(),
            base_fee_eligibility: BTreeMap::new(),
            priority_queue: BTreeMap::new(),
            current_base_fee: config.initial_base_fee,
            private_flow_ratio: config.private_flow_prior,
            last_block_gas_limit: 30_000_000, // Default value, will be updated by first block
            last_block_number: None,
            last_block_hash: None,
            history: VecDeque::with_capacity(32),
            config,
        };

        for tx in initial_txs {
            inner.insert(tx.clone());
        }

        let inner_arc = Arc::new(RwLock::new(inner));

        let handle = MempoolHandle {
            inner: inner_arc.clone(),
        };

        let tracker = MempoolTracker {
            inner: inner_arc,
            rx,
        };

        (handle, tracker)
    }

    /// Creates a new mempool tracker and its handle.
    pub fn new(
        rx: Receiver<MempoolEvent>,
        initial_txs: &[PendingTx],
        config: TrackerConfig,
    ) -> (MempoolHandle, MempoolTracker) {
        Self::from_channel(rx, initial_txs, config)
    }

    /// Creates a tracker runtime from an authenticated Alloy provider builder and websocket connector.
    pub async fn connect_with_builder<L, F>(
        builder: alloy::providers::ProviderBuilder<L, F>,
        ws: alloy::providers::WsConnect,
        config: TrackerConfig,
    ) -> Result<TrackerRuntime, TrackerError>
    where
        L: alloy::providers::ProviderLayer<
                alloy::providers::RootProvider,
                alloy::network::Ethereum,
            >,
        F: TxFiller<alloy::network::Ethereum>
            + alloy::providers::ProviderLayer<L::Provider, alloy::network::Ethereum>,
        F::Provider: 'static,
    {
        rpc::connect_with_builder(builder, ws, config).await
    }

    /// Creates a tracker runtime from an existing Alloy pubsub-capable provider.
    pub async fn connect_with_provider<P>(
        provider: P,
        config: TrackerConfig,
    ) -> Result<TrackerRuntime, TrackerError>
    where
        P: alloy::providers::Provider<alloy::network::Ethereum> + 'static,
    {
        rpc::connect_with_provider(provider, config).await
    }

    /// Runs the tracker's event loop.
    pub fn run(self) {
        loop {
            match self.rx.recv() {
                Ok(MempoolEvent::PendingTransaction(tx)) => {
                    let mut inner = self.inner.write().unwrap();
                    inner.insert(tx);
                }
                Ok(MempoolEvent::Prune(prune)) => {
                    let mut inner = self.inner.write().unwrap();
                    inner.prune_and_reanchor(prune.anchor, prune.future_blocks);
                }
                Ok(MempoolEvent::NewBlock(block)) => {
                    let mut inner = self.inner.write().unwrap();
                    inner.apply_block(block);
                }
                Ok(MempoolEvent::FinalizedBlock(number)) => {
                    // For now we just log it or we could use it to prune old state if needed
                    tracing::info!(block_number = number.0, "Block finalized");
                }
                Err(_) => break, // sender dropped, shut down cleanly
            }
        }
    }
}

impl MempoolInner {
    fn prune_and_reanchor(&mut self, anchor: BlockUpdate, future_blocks: Vec<BlockUpdate>) {
        // 1. Build a map of latest nonces from the anchor block and any future blocks
        let mut latest_nonces = HashMap::new();
        for tx in &anchor.included_txs {
            let entry = latest_nonces.entry(tx.sender).or_insert(0u64);
            *entry = (*entry).max(tx.nonce + 1);
        }
        for block in future_blocks {
            for tx in &block.included_txs {
                let entry = latest_nonces.entry(tx.sender).or_insert(0u64);
                *entry = (*entry).max(tx.nonce + 1);
            }
        }

        // 2. Identify transactions to keep
        let now = SystemTime::now();
        let one_hour = std::time::Duration::from_secs(3600);
        let mut to_keep = Vec::new();

        for (addr, queue) in &self.account_queues {
            for tx in queue.slots.iter().flatten() {
                let mut keep = false;

                // Keep if nonce is greater than any we've seen in the new chain head/future
                if let Some(&latest) = latest_nonces.get(addr) {
                    if tx.nonce >= latest {
                        keep = true;
                    }
                } else {
                    // Account not seen in the new blocks, keep it for now
                    keep = true;
                }

                // Keep if it's been in the mempool for a long time (> 1 hour)
                if let Ok(age) = now.duration_since(tx.seen_at)
                    && age > one_hour
                {
                    keep = true;
                }

                // Keep if it's not paying enough for the new base fee (unlikely to have been included)
                if tx.max_fee_per_gas < anchor.new_base_fee {
                    keep = true;
                }

                if keep {
                    to_keep.push(tx.clone());
                }
            }
        }

        // 3. Reset internal state but preserve configuration
        self.account_queues.clear();
        self.base_fee_eligibility.clear();
        self.priority_queue.clear();
        self.current_base_fee = anchor.new_base_fee;
        self.last_block_gas_limit = anchor.gas_limit;
        self.last_block_number = Some(anchor.number);
        self.last_block_hash = Some(anchor.hash);
        self.history.clear();

        // Add anchor to history
        let prev_base_fee = self.current_base_fee; // simplified
        let prev_private_flow_ratio = self.private_flow_ratio;
        let prev_last_block_gas_limit = self.last_block_gas_limit;

        self.history.push_back(HistoryEntry {
            block: anchor,
            removed_txs: Vec::new(),
            prev_base_fee,
            prev_private_flow_ratio,
            prev_last_block_gas_limit,
            prev_confirmed_nonces: HashMap::new(),
        });

        // 4. Re-insert the "kept" transactions
        for tx in to_keep {
            self.insert(tx);
        }
    }

    fn effective_priority_fee(&self, tx: &PendingTx) -> u128 {
        tx.max_priority_fee_per_gas
            .min(tx.max_fee_per_gas.saturating_sub(self.current_base_fee))
    }

    fn insert(&mut self, tx: PendingTx) {
        // Global capacity check and eviction
        if self.base_fee_eligibility.len() >= self.config.global_capacity {
            let effective_priority_fee = self.effective_priority_fee(&tx);

            if let Some((key, _)) = self.priority_queue.iter().next() {
                let (worst_fee, _, _) = key;
                if effective_priority_fee <= worst_fee.0 {
                    return;
                }
            }

            if let Some(((_worst_fee, worst_addr, worst_nonce), _)) =
                self.priority_queue.pop_first()
            {
                if let Some(evicted_account_queue) = self.account_queues.get_mut(&worst_addr) {
                    let nonce_offset =
                        (worst_nonce - evicted_account_queue.confirmed_nonce) as usize;
                    if let Some(Some(evicted_tx)) =
                        evicted_account_queue.slots.get_mut(nonce_offset)
                    {
                        self.base_fee_eligibility.remove(&(
                            evicted_tx.max_fee_per_gas,
                            worst_addr,
                            worst_nonce,
                        ));
                        evicted_account_queue.slots[nonce_offset] = None;
                    }
                }
            } else {
                return;
            }
        }

        // Get or create the account queue.
        let account_queue = self.account_queues.entry(tx.sender).or_insert_with(|| {
            let confirmed_nonce = tx.nonce;
            AccountQueue {
                confirmed_nonce,
                slots: Vec::new(),
            }
        });

        if tx.nonce < account_queue.confirmed_nonce {
            let prepend = (account_queue.confirmed_nonce - tx.nonce) as usize;
            if prepend + account_queue.slots.len() > self.config.per_account_capacity {
                return;
            }

            let mut shifted_slots = vec![None; prepend];
            shifted_slots.append(&mut account_queue.slots);
            account_queue.slots = shifted_slots;
            account_queue.confirmed_nonce = tx.nonce;
        }

        // Per-account capacity check
        let nonce_offset = (tx.nonce - account_queue.confirmed_nonce) as usize;
        if nonce_offset >= self.config.per_account_capacity {
            return;
        }

        // Resize account slots if needed
        let slot_index = (tx.nonce - account_queue.confirmed_nonce) as usize;
        if slot_index >= account_queue.slots.len() {
            account_queue.slots.resize(slot_index + 1, None);
        }

        // Insert into account queue
        account_queue.slots[slot_index] = Some(tx.clone());

        // Insert into indexes
        self.base_fee_eligibility
            .insert((tx.max_fee_per_gas, tx.sender, tx.nonce), ());
        let effective_priority_fee = self.effective_priority_fee(&tx);
        self.priority_queue
            .insert((Reverse(effective_priority_fee), tx.sender, tx.nonce), ());
    }

    fn apply_block(&mut self, block: BlockUpdate) {
        // Detect reorg
        if let Some(last_hash) = self.last_block_hash
            && block.parent_hash != last_hash
        {
            tracing::info!(
                block_number = block.number.0,
                "Reorg detected, parent hash mismatch"
            );
            self.handle_reorg(block);
            return;
        }

        // Detect gap
        if let Some(last_num) = self.last_block_number
            && block.number.0 != last_num.0 + 1
        {
            tracing::warn!(
                expected = last_num.0 + 1,
                received = block.number.0,
                "Non-contiguous block received"
            );
            // The transport layer should handle recovery/resets
            return;
        }

        self.attach_block(block);
    }

    fn attach_block(&mut self, block: BlockUpdate) {
        let prev_base_fee = self.current_base_fee;
        let prev_private_flow_ratio = self.private_flow_ratio;
        let prev_last_block_gas_limit = self.last_block_gas_limit;
        let mut prev_confirmed_nonces = HashMap::new();

        // 1. Private order flow estimation
        let mut known_gas_used: u64 = 0;
        for tx in &block.included_txs {
            if let Some(account_queue) = self.account_queues.get(&tx.sender)
                && tx.nonce >= account_queue.confirmed_nonce
            {
                let nonce_offset = (tx.nonce - account_queue.confirmed_nonce) as usize;
                if let Some(Some(_)) = account_queue.slots.get(nonce_offset) {
                    known_gas_used += tx.gas_limit;
                }
            }
        }

        if block.gas_limit > 0 {
            let total_gas_used = block.gas_used;
            let private_gas = total_gas_used.saturating_sub(known_gas_used);
            let observed_private_flow = private_gas as f64 / block.gas_limit as f64;

            let alpha = 0.1;
            self.private_flow_ratio =
                alpha * observed_private_flow + (1.0 - alpha) * self.private_flow_ratio;
        }
        self.last_block_gas_limit = block.gas_limit;

        // 2. Remove confirmed transactions from indexes and account queues
        let mut removed_txs = Vec::new();
        for tx_in_block in &block.included_txs {
            if let Some(account_queue) = self.account_queues.get_mut(&tx_in_block.sender)
                && tx_in_block.nonce >= account_queue.confirmed_nonce
            {
                let nonce_offset = (tx_in_block.nonce - account_queue.confirmed_nonce) as usize;
                if nonce_offset < account_queue.slots.len()
                    && let Some(mempool_tx) = account_queue.slots[nonce_offset].take()
                {
                    self.base_fee_eligibility.remove(&(
                        mempool_tx.max_fee_per_gas,
                        mempool_tx.sender,
                        mempool_tx.nonce,
                    ));
                    let old_effective_priority = self.effective_priority_fee(&mempool_tx);
                    self.priority_queue.remove(&(
                        Reverse(old_effective_priority),
                        mempool_tx.sender,
                        mempool_tx.nonce,
                    ));
                    removed_txs.push(mempool_tx);
                }
            }
        }

        // 3. Update confirmed nonces and drain slots
        for tx in &block.included_txs {
            if let Some(account_queue) = self.account_queues.get_mut(&tx.sender) {
                let old_confirmed_nonce = account_queue.confirmed_nonce;
                prev_confirmed_nonces
                    .entry(tx.sender)
                    .or_insert(old_confirmed_nonce);

                account_queue.confirmed_nonce = account_queue.confirmed_nonce.max(tx.nonce + 1);

                let drain_count = (account_queue.confirmed_nonce - old_confirmed_nonce) as usize;
                if drain_count > 0 {
                    if drain_count < account_queue.slots.len() {
                        account_queue.slots.drain(0..drain_count);
                    } else {
                        account_queue.slots.clear();
                    }
                }
            }
        }

        // 4. Update base fee
        self.current_base_fee = block.new_base_fee;

        // 5. Update history and head
        self.last_block_number = Some(block.number);
        self.last_block_hash = Some(block.hash);

        if self.history.len() == 32 {
            self.history.pop_front();
        }
        self.history.push_back(HistoryEntry {
            block,
            removed_txs,
            prev_base_fee,
            prev_private_flow_ratio,
            prev_last_block_gas_limit,
            prev_confirmed_nonces,
        });

        // 6. Re-evaluate priority
        self.rebuild_priority_queue();
    }

    fn detach_block(&mut self) -> Option<Vec<PendingTx>> {
        let entry = self.history.pop_back()?;

        // 1. Restore previous state
        self.current_base_fee = entry.prev_base_fee;
        self.private_flow_ratio = entry.prev_private_flow_ratio;
        self.last_block_gas_limit = entry.prev_last_block_gas_limit;

        if let Some(new_last) = self.history.back() {
            self.last_block_number = Some(new_last.block.number);
            self.last_block_hash = Some(new_last.block.hash);
        } else {
            self.last_block_number = None;
            self.last_block_hash = None;
        }

        // 2. Restore confirmed nonces and prepend empty slots
        for (addr, prev_nonce) in entry.prev_confirmed_nonces {
            if let Some(account_queue) = self.account_queues.get_mut(&addr) {
                let current_nonce = account_queue.confirmed_nonce;
                if current_nonce > prev_nonce {
                    let prepend_count = (current_nonce - prev_nonce) as usize;
                    let mut new_slots = vec![None; prepend_count];
                    new_slots.append(&mut account_queue.slots);
                    account_queue.slots = new_slots;
                    account_queue.confirmed_nonce = prev_nonce;
                }
            }
        }

        // 3. Restore removed transactions to the correct slots
        for tx in &entry.removed_txs {
            if let Some(account_queue) = self.account_queues.get_mut(&tx.sender) {
                let offset = (tx.nonce - account_queue.confirmed_nonce) as usize;
                if offset < account_queue.slots.len() {
                    account_queue.slots[offset] = Some(tx.clone());

                    // Re-insert into eligibility index
                    self.base_fee_eligibility
                        .insert((tx.max_fee_per_gas, tx.sender, tx.nonce), ());
                }
            }
        }

        // 4. Rebuild priority queue
        self.rebuild_priority_queue();

        Some(entry.removed_txs)
    }

    fn rebuild_priority_queue(&mut self) {
        self.priority_queue.clear();

        for account_queue in self.account_queues.values() {
            let mut nonce_bound = false;
            for slot in account_queue.slots.iter() {
                if let Some(tx) = slot {
                    if nonce_bound {
                        continue;
                    }

                    if tx.max_fee_per_gas < self.current_base_fee {
                        nonce_bound = true;
                        continue;
                    }

                    let effective_priority_fee = self.effective_priority_fee(tx);
                    if effective_priority_fee > 0 {
                        self.priority_queue
                            .insert((Reverse(effective_priority_fee), tx.sender, tx.nonce), ());
                    } else {
                        nonce_bound = true;
                    }
                } else {
                    nonce_bound = true;
                }
            }
        }
    }

    fn handle_reorg(&mut self, new_block: BlockUpdate) {
        // 1. Find common ancestor in history
        let mut ancestor_index = None;
        for (i, entry) in self.history.iter().enumerate().rev() {
            if entry.block.hash == new_block.parent_hash {
                ancestor_index = Some(i);
                break;
            }
        }

        let Some(index) = ancestor_index else {
            tracing::warn!(
                block_number = new_block.number.0,
                parent_hash = ?new_block.parent_hash,
                "Common ancestor not found in history, performing full reset"
            );
            // If we can't find the ancestor in our 32-block window, we must reset
            self.prune_and_reanchor(new_block, vec![]);
            return;
        };

        // 2. Detach blocks back to the common ancestor
        let mut restore_set = HashMap::new();
        while self.history.len() > index + 1 {
            if let Some(removed) = self.detach_block() {
                for tx in removed {
                    restore_set.insert(tx.id, tx);
                }
            }
        }

        // 3. Attach the new block (and any others if we had them)
        // For now we only have the one new_block that triggered this
        self.attach_block(new_block);

        // 4. Re-insert transactions from the restore set that weren't in the new block(s)
        // attach_block already re-removed them if they were in the new block.
        // We just need to put back what's left.
        for (_, tx) in restore_set {
            self.insert(tx);
        }
    }

    fn classify_transaction(&self, tx_to_classify: &PendingTx) -> TxClassification {
        if let Some(account_queue) = self.account_queues.get(&tx_to_classify.sender) {
            for i in 0..((tx_to_classify.nonce - account_queue.confirmed_nonce) as usize) {
                if let Some(Some(tx)) = account_queue.slots.get(i) {
                    if tx.max_fee_per_gas < self.current_base_fee {
                        return TxClassification::NonceBound;
                    }
                    let effective_priority_fee = self.effective_priority_fee(tx);
                    if effective_priority_fee == 0 {
                        return TxClassification::NonceBound;
                    }
                } else {
                    return TxClassification::NonceBound; // Nonce gap
                }
            }

            if tx_to_classify.max_fee_per_gas < self.current_base_fee {
                return TxClassification::BaseFeeInvalid;
            }
            let effective_priority_fee = self.effective_priority_fee(tx_to_classify);
            if effective_priority_fee == 0 {
                return TxClassification::Unmarketable;
            }

            return TxClassification::Marketable;
        }
        TxClassification::NonceBound
    }

    fn find_tx_by_id(&self, id: &TxId) -> Option<PendingTx> {
        for account_queue in self.account_queues.values() {
            for slot in &account_queue.slots {
                if let Some(tx) = slot
                    && tx.id == *id
                {
                    return Some(tx.clone());
                }
            }
        }
        None
    }

    fn find_tx_by_addr_and_nonce(&self, addr: Address, nonce: u64) -> Option<PendingTx> {
        if let Some(account_queue) = self.account_queues.get(&addr)
            && nonce >= account_queue.confirmed_nonce
        {
            let offset = (nonce - account_queue.confirmed_nonce) as usize;
            if let Some(Some(tx)) = account_queue.slots.get(offset) {
                return Some(tx.clone());
            }
        }
        None
    }
}

/// A handle to the mempool tracker, which can be cloned and shared across threads.
#[derive(Clone)]
pub struct MempoolHandle {
    inner: Arc<RwLock<MempoolInner>>,
}

impl MempoolHandle {
    /// Returns the classification of a transaction.
    pub fn classification(&self, id: &TxId) -> Option<TxClassification> {
        let inner = self.inner.read().unwrap();
        inner
            .find_tx_by_id(id)
            .as_ref()
            .map(|tx| inner.classify_transaction(tx))
    }

    /// Estimates the number of blocks until a transaction is confirmed.
    pub fn estimated_blocks_to_confirm(&self, id: &TxId) -> Option<u64> {
        let inner = self.inner.read().unwrap();
        let tx = inner.find_tx_by_id(id)?;
        let gas_ahead = self.gas_ahead(id)?;

        let usable_capacity =
            (inner.last_block_gas_limit as f64 * (1.0 - inner.private_flow_ratio)) as u64;
        if usable_capacity == 0 {
            return Some(u64::MAX);
        }

        Some(gas_ahead.saturating_add(tx.gas_limit) / usable_capacity)
    }

    /// Returns the total gas of transactions with a higher effective priority fee.
    pub fn gas_ahead(&self, id: &TxId) -> Option<u64> {
        let inner = self.inner.read().unwrap();
        let tx = inner.find_tx_by_id(id)?;

        if inner.classify_transaction(&tx) != TxClassification::Marketable {
            return None;
        }

        let effective_priority_fee = inner.effective_priority_fee(&tx);
        let key_for_tx = (Reverse(effective_priority_fee), tx.sender, tx.nonce);

        let mut gas_ahead: u64 = 0;
        for (key, _) in inner.priority_queue.range(..key_for_tx) {
            let (_, addr, nonce) = key;
            if let Some(tx_ahead) = inner.find_tx_by_addr_and_nonce(*addr, *nonce) {
                gas_ahead += tx_ahead.gas_limit;
            }
        }
        Some(gas_ahead)
    }

    /// Returns true if the transaction is currently marketable.
    pub fn is_marketable(&self, id: &TxId) -> Option<bool> {
        self.classification(id)
            .map(|c| c == TxClassification::Marketable)
    }

    /// Returns the current base fee.
    pub fn current_base_fee(&self) -> u128 {
        self.inner.read().unwrap().current_base_fee
    }

    /// Returns the current private flow ratio estimate.
    pub fn private_flow_ratio(&self) -> f64 {
        self.inner.read().unwrap().private_flow_ratio
    }

    /// Returns the priority queue for inspection.
    pub fn priority_queue(&self) -> Vec<(u128, Address, u64)> {
        let inner = self.inner.read().unwrap();
        inner
            .priority_queue
            .iter()
            .map(|((fee, addr, nonce), _)| (fee.0, *addr, *nonce))
            .collect()
    }

    /// Returns the gas limit of the last block.
    pub fn last_block_gas_limit(&self) -> u64 {
        self.inner.read().unwrap().last_block_gas_limit
    }

    /// Finds a transaction by its address and nonce.
    pub fn find_tx_by_addr_and_nonce(&self, addr: Address, nonce: u64) -> Option<PendingTx> {
        self.inner
            .read()
            .unwrap()
            .find_tx_by_addr_and_nonce(addr, nonce)
    }
}

/// The classification of a transaction in the mempool.
#[derive(Debug, PartialEq, Eq)]
pub enum TxClassification {
    /// The transaction is likely to be included in a block.
    Marketable,
    /// The transaction's max fee is less than the current base fee.
    BaseFeeInvalid,
    /// The transaction can pay the base fee, but has no tip.
    Unmarketable,
    /// The transaction is blocked by a nonce gap or an unmarketable transaction from the same sender.
    NonceBound,
}
