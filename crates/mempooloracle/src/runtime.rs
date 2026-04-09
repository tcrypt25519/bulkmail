use crate::MempoolHandle;
use std::sync::{
    Arc,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};
use tokio::{sync::watch, task::JoinHandle};

pub(crate) const DEFAULT_SHUTDOWN_VALUE: bool = false;

pub struct TrackerRuntime {
    handle: MempoolHandle,
    telemetry: Arc<RuntimeTelemetry>,
    shutdown: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
}

impl TrackerRuntime {
    pub(crate) fn new(
        handle: MempoolHandle,
        telemetry: Arc<RuntimeTelemetry>,
        shutdown: watch::Sender<bool>,
        tasks: Vec<JoinHandle<()>>,
    ) -> Self {
        Self {
            handle,
            telemetry,
            shutdown,
            tasks,
        }
    }

    pub fn handle(&self) -> MempoolHandle {
        self.handle.clone()
    }

    pub fn telemetry(&self) -> TrackerTelemetry {
        TrackerTelemetry {
            inner: self.telemetry.clone(),
        }
    }
}

impl Drop for TrackerRuntime {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        for task in &self.tasks {
            task.abort();
        }
    }
}

#[derive(Clone)]
pub struct TrackerTelemetry {
    inner: Arc<RuntimeTelemetry>,
}

impl TrackerTelemetry {
    pub fn snapshot(&self) -> TrackerTelemetrySnapshot {
        TrackerTelemetrySnapshot {
            transport_kind: self.inner.transport_kind,
            pending_seen: self.inner.pending_seen.load(Ordering::Relaxed),
            block_count: self.inner.block_count.load(Ordering::Relaxed),
            last_block_txs: self.inner.last_block_txs.load(Ordering::Relaxed),
            last_block_gas_used: self.inner.last_block_gas_used.load(Ordering::Relaxed),
            initial_backfill_txs: self.inner.initial_backfill_txs.load(Ordering::Relaxed),
            p2p_peer_count: self.inner.p2p_peer_count.load(Ordering::Relaxed),
            consensus_peer_count: self.inner.consensus_peer_count.load(Ordering::Relaxed),
            p2p_announced_txs: self.inner.p2p_announced_txs.load(Ordering::Relaxed),
            p2p_imported_txs: self.inner.p2p_imported_txs.load(Ordering::Relaxed),
            consensus_anchor_block_number: self
                .inner
                .consensus_anchor_block_number
                .load(Ordering::Relaxed),
            consensus_next_expected_block_number: self
                .inner
                .consensus_next_expected_block_number
                .load(Ordering::Relaxed),
            consensus_last_block_number: self
                .inner
                .consensus_last_block_number
                .load(Ordering::Relaxed),
            consensus_gap_resets: self.inner.consensus_gap_resets.load(Ordering::Relaxed),
            consensus_recovered_blocks: self
                .inner
                .consensus_recovered_blocks
                .load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TransportKind {
    #[default]
    Rpc,
    P2p,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TrackerTelemetrySnapshot {
    pub transport_kind: TransportKind,
    pub pending_seen: u64,
    pub block_count: u64,
    pub last_block_txs: usize,
    pub last_block_gas_used: u64,
    pub initial_backfill_txs: usize,
    pub p2p_peer_count: usize,
    pub consensus_peer_count: usize,
    pub p2p_announced_txs: u64,
    pub p2p_imported_txs: u64,
    pub consensus_anchor_block_number: u64,
    pub consensus_next_expected_block_number: u64,
    pub consensus_last_block_number: u64,
    pub consensus_gap_resets: u64,
    pub consensus_recovered_blocks: u64,
}

pub(crate) struct RuntimeTelemetry {
    transport_kind: TransportKind,
    pending_seen: AtomicU64,
    block_count: AtomicU64,
    last_block_txs: AtomicUsize,
    last_block_gas_used: AtomicU64,
    initial_backfill_txs: AtomicUsize,
    p2p_peer_count: AtomicUsize,
    consensus_peer_count: AtomicUsize,
    p2p_announced_txs: AtomicU64,
    p2p_imported_txs: AtomicU64,
    consensus_anchor_block_number: AtomicU64,
    consensus_next_expected_block_number: AtomicU64,
    consensus_last_block_number: AtomicU64,
    consensus_gap_resets: AtomicU64,
    consensus_recovered_blocks: AtomicU64,
}

impl RuntimeTelemetry {
    pub(crate) fn new(transport_kind: TransportKind) -> Self {
        Self {
            transport_kind,
            pending_seen: AtomicU64::default(),
            block_count: AtomicU64::default(),
            last_block_txs: AtomicUsize::default(),
            last_block_gas_used: AtomicU64::default(),
            initial_backfill_txs: AtomicUsize::default(),
            p2p_peer_count: AtomicUsize::default(),
            consensus_peer_count: AtomicUsize::default(),
            p2p_announced_txs: AtomicU64::default(),
            p2p_imported_txs: AtomicU64::default(),
            consensus_anchor_block_number: AtomicU64::default(),
            consensus_next_expected_block_number: AtomicU64::default(),
            consensus_last_block_number: AtomicU64::default(),
            consensus_gap_resets: AtomicU64::default(),
            consensus_recovered_blocks: AtomicU64::default(),
        }
    }

    pub(crate) fn record_backfill_size(&self, txs: usize) {
        self.initial_backfill_txs.store(txs, Ordering::Relaxed);
    }

    pub(crate) fn record_pending_seen(&self) {
        self.pending_seen.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_block(&self, txs: usize, gas_used: u64) {
        self.block_count.fetch_add(1, Ordering::Relaxed);
        self.last_block_txs.store(txs, Ordering::Relaxed);
        self.last_block_gas_used.store(gas_used, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub(crate) fn record_p2p_peer_count(&self, peers: usize) {
        self.p2p_peer_count.store(peers, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub(crate) fn record_consensus_peer_count(&self, peers: usize) {
        self.consensus_peer_count.store(peers, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub(crate) fn record_p2p_announcement(&self) {
        self.p2p_announced_txs.fetch_add(1, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub(crate) fn record_p2p_import(&self) {
        self.p2p_imported_txs.fetch_add(1, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub(crate) fn record_consensus_anchor(&self, block_number: u64) {
        self.consensus_anchor_block_number
            .store(block_number, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub(crate) fn record_consensus_next_expected(&self, block_number: u64) {
        self.consensus_next_expected_block_number
            .store(block_number, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub(crate) fn record_consensus_last_block(&self, block_number: u64) {
        self.consensus_last_block_number
            .store(block_number, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub(crate) fn record_consensus_gap_reset(&self) {
        self.consensus_gap_resets.fetch_add(1, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub(crate) fn record_consensus_recovered_blocks(&self, recovered: u64) {
        self.consensus_recovered_blocks
            .fetch_add(recovered, Ordering::Relaxed);
    }
}
