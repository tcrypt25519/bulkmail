//! A tracker for EIP-1559 mempool state and transaction confirmation latency.
mod alloy_support;

use alloy::providers::fillers::TxFiller;
use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, RwLock, mpsc::Receiver};

pub use alloy_support::{AlloyTrackerError, AlloyTrackerRuntime};

/// A unique identifier for a transaction.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct TxId(pub [u8; 32]);

/// A unique identifier for an account.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Address(pub [u8; 20]);

/// An event that drives the mempool tracker.
#[derive(Debug, Clone)]
pub enum MempoolEvent {
    /// A new transaction has entered the mempool.
    PendingTransaction(PendingTx),
    /// A new block has been mined.
    NewBlock(BlockUpdate),
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
}

/// An update about a new block.
#[derive(Debug, Clone)]
pub struct BlockUpdate {
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
    ) -> Result<AlloyTrackerRuntime, AlloyTrackerError>
    where
        L: alloy::providers::ProviderLayer<
                alloy::providers::RootProvider,
                alloy::network::Ethereum,
            >,
        F: TxFiller<alloy::network::Ethereum>
            + alloy::providers::ProviderLayer<L::Provider, alloy::network::Ethereum>,
        F::Provider: 'static,
    {
        alloy_support::connect_with_builder(builder, ws, config).await
    }

    /// Creates a tracker runtime from an existing Alloy pubsub-capable provider.
    pub async fn connect_with_provider<P>(
        provider: P,
        config: TrackerConfig,
    ) -> Result<AlloyTrackerRuntime, AlloyTrackerError>
    where
        P: alloy::providers::Provider<alloy::network::Ethereum> + 'static,
    {
        alloy_support::connect_with_provider(provider, config).await
    }

    /// Runs the tracker's event loop.
    pub fn run(self) {
        loop {
            match self.rx.recv() {
                Ok(MempoolEvent::PendingTransaction(tx)) => {
                    let mut inner = self.inner.write().unwrap();
                    inner.insert(tx);
                }
                Ok(MempoolEvent::NewBlock(block)) => {
                    let mut inner = self.inner.write().unwrap();
                    inner.apply_block(block);
                }
                Err(_) => break, // sender dropped, shut down cleanly
            }
        }
    }
}

impl MempoolInner {
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
        // 1. Private order flow estimation
        let mut known_gas_used: u64 = 0;
        for tx in &block.included_txs {
            if let Some(account_queue) = self.account_queues.get(&tx.sender) {
                if tx.nonce >= account_queue.confirmed_nonce {
                    let nonce_offset = (tx.nonce - account_queue.confirmed_nonce) as usize;
                    if let Some(Some(_)) = account_queue.slots.get(nonce_offset) {
                        known_gas_used += tx.gas_limit;
                    }
                }
            }
        }

        if block.gas_limit > 0 {
            let total_gas_used = block.gas_used;
            // Ensure known_gas_used does not exceed total_gas_used
            let private_gas = total_gas_used.saturating_sub(known_gas_used);
            let observed_private_flow = private_gas as f64 / block.gas_limit as f64;

            // EMA update
            let alpha = 0.1;
            self.private_flow_ratio =
                alpha * observed_private_flow + (1.0 - alpha) * self.private_flow_ratio;
        }
        self.last_block_gas_limit = block.gas_limit;

        // 2. Remove confirmed transactions from indexes and account queues
        for tx_in_block in &block.included_txs {
            if let Some(account_queue) = self.account_queues.get_mut(&tx_in_block.sender) {
                if tx_in_block.nonce >= account_queue.confirmed_nonce {
                    let nonce_offset = (tx_in_block.nonce - account_queue.confirmed_nonce) as usize;
                    if nonce_offset < account_queue.slots.len() {
                        // Take the transaction from the slot to get ownership and remove it from the B-Trees.
                        // This also sets the slot to None, achieving the goal of the original code.
                        if let Some(mempool_tx) = account_queue.slots[nonce_offset].take() {
                            self.base_fee_eligibility.remove(&(
                                mempool_tx.max_fee_per_gas,
                                mempool_tx.sender,
                                mempool_tx.nonce,
                            ));
                            // The effective_priority_fee must be calculated with the *old* base_fee to find it in the priority_queue.
                            let old_effective_priority = self.effective_priority_fee(&mempool_tx);
                            self.priority_queue.remove(&(
                                Reverse(old_effective_priority),
                                mempool_tx.sender,
                                mempool_tx.nonce,
                            ));
                        }
                    }
                }
            }
        }

        // 3. Update confirmed nonces and drain slots
        for tx in &block.included_txs {
            if let Some(account_queue) = self.account_queues.get_mut(&tx.sender) {
                let old_confirmed_nonce = account_queue.confirmed_nonce;
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

        // 5. Re-evaluate priority for all transactions and rebuild priority queue
        // The base_fee_eligibility map is already up-to-date.
        self.priority_queue.clear();

        for account_queue in self.account_queues.values() {
            let mut nonce_bound = false;
            for slot in account_queue.slots.iter() {
                if let Some(tx) = slot {
                    // No need to re-insert into base_fee_eligibility, it's already correct.

                    if nonce_bound {
                        continue;
                    }

                    if tx.max_fee_per_gas < self.current_base_fee {
                        nonce_bound = true;
                        continue;
                    }

                    let effective_priority_fee = self.effective_priority_fee(&tx);
                    if effective_priority_fee > 0 {
                        self.priority_queue
                            .insert((Reverse(effective_priority_fee), tx.sender, tx.nonce), ());
                    } else {
                        nonce_bound = true;
                    }
                } else {
                    nonce_bound = true; // Nonce gap
                }
            }
        }
    }

    fn classify_transaction(&self, tx_to_classify: &PendingTx) -> TxClassification {
        if let Some(account_queue) = self.account_queues.get(&tx_to_classify.sender) {
            for i in 0..((tx_to_classify.nonce - account_queue.confirmed_nonce) as usize) {
                if let Some(Some(tx)) = account_queue.slots.get(i) {
                    if tx.max_fee_per_gas < self.current_base_fee {
                        return TxClassification::NonceBound;
                    }
                    let effective_priority_fee = self.effective_priority_fee(&tx);
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
                if let Some(tx) = slot {
                    if tx.id == *id {
                        return Some(tx.clone());
                    }
                }
            }
        }
        None
    }

    fn find_tx_by_addr_and_nonce(&self, addr: Address, nonce: u64) -> Option<PendingTx> {
        if let Some(account_queue) = self.account_queues.get(&addr) {
            if nonce >= account_queue.confirmed_nonce {
                let offset = (nonce - account_queue.confirmed_nonce) as usize;
                if let Some(Some(tx)) = account_queue.slots.get(offset) {
                    return Some(tx.clone());
                }
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
