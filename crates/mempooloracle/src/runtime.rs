use crate::{MempoolHandle, Slot, BlockNumber};
use std::collections::HashSet;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};
use tokio::{sync::watch, task::JoinHandle};
use metrics::{counter, gauge};
use metrics_exporter_prometheus::PrometheusBuilder;

pub(crate) const DEFAULT_SHUTDOWN_VALUE: bool = false;

/// Initializes the global Prometheus metrics exporter.
pub fn init_metrics(port: u16) -> Result<(), String> {
    let builder = PrometheusBuilder::new();
    builder
        .with_http_listener(([0, 0, 0, 0], port))
        .install()
        .map_err(|e| e.to_string())
}

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
        // Spawn prometheus updater task
        let tel_clone = telemetry.clone();
        let mut shutdown_rx = shutdown.subscribe();
        let prometheus_task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        tel_clone.update_prometheus();
                    }
                    _ = shutdown_rx.changed() => break,
                }
            }
        });

        let mut all_tasks = tasks;
        all_tasks.push(prometheus_task);

        Self {
            handle,
            telemetry,
            shutdown,
            tasks: all_tasks,
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
            consensus_anchor_block_number: BlockNumber(self
                .inner
                .consensus_anchor_block_number
                .load(Ordering::Relaxed)),
            consensus_next_expected_block_number: BlockNumber(self
                .inner
                .consensus_next_expected_block_number
                .load(Ordering::Relaxed)),
            consensus_last_block_number: BlockNumber(self
                .inner
                .consensus_last_block_number
                .load(Ordering::Relaxed)),
            consensus_gap_resets: self.inner.consensus_gap_resets.load(Ordering::Relaxed),
            consensus_recovered_blocks: self
                .inner
                .consensus_recovered_blocks
                .load(Ordering::Relaxed),
            consensus_finalized_slot: Slot(self
                .inner
                .consensus_finalized_slot
                .load(Ordering::Relaxed)),
            consensus_finalized_number: BlockNumber(self
                .inner
                .consensus_finalized_number
                .load(Ordering::Relaxed)),

            el_connections_ingress: self.inner.el_connections_ingress.load(Ordering::Relaxed),
            el_connections_egress: self.inner.el_connections_egress.load(Ordering::Relaxed),
            cl_connections_ingress: self.inner.cl_connections_ingress.load(Ordering::Relaxed),
            cl_connections_egress: self.inner.cl_connections_egress.load(Ordering::Relaxed),
            el_active_connections_ingress: self.inner.el_active_connections_ingress.load(Ordering::Relaxed) as u64,
            el_active_connections_egress: self.inner.el_active_connections_egress.load(Ordering::Relaxed) as u64,
            cl_active_connections_ingress: self.inner.cl_active_connections_ingress.load(Ordering::Relaxed) as u64,
            cl_active_connections_egress: self.inner.cl_active_connections_egress.load(Ordering::Relaxed) as u64,
            el_unique_peers_ingress: self.inner.el_unique_peers_ingress.lock().unwrap().len() as u64,
            el_unique_peers_egress: self.inner.el_unique_peers_egress.lock().unwrap().len() as u64,
            cl_unique_peers_ingress: self.inner.cl_unique_peers_ingress.lock().unwrap().len() as u64,
            cl_unique_peers_egress: self.inner.cl_unique_peers_egress.lock().unwrap().len() as u64,

            el_tx_hashes_received: self.inner.el_tx_hashes_received.load(Ordering::Relaxed),
            el_txs_received: self.inner.el_txs_received.load(Ordering::Relaxed),
            cl_blocks_received: self.inner.cl_blocks_received.load(Ordering::Relaxed),
            cl_finality_updates_received: self.inner.cl_finality_updates_received.load(Ordering::Relaxed),
            cl_status_received: self.inner.cl_status_received.load(Ordering::Relaxed),
            cl_status_sent: self.inner.cl_status_sent.load(Ordering::Relaxed),
            cl_blocks_by_range_requests_sent: self.inner.cl_blocks_by_range_requests_sent.load(Ordering::Relaxed),
            cl_blocks_by_range_responses_received: self.inner.cl_blocks_by_range_responses_received.load(Ordering::Relaxed),

            cl_blocks_by_range_latency_avg_ns: {
                let sum = self.inner.cl_blocks_by_range_latency_sum.load(Ordering::Relaxed);
                let count = self.inner.cl_blocks_by_range_latency_count.load(Ordering::Relaxed);
                sum.checked_div(count).unwrap_or(0)
            },
            cl_status_latency_avg_ns: {
                let sum = self.inner.cl_status_latency_sum.load(Ordering::Relaxed);
                let count = self.inner.cl_status_latency_count.load(Ordering::Relaxed);
                sum.checked_div(count).unwrap_or(0)
            },
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
    pub consensus_anchor_block_number: BlockNumber,
    pub consensus_next_expected_block_number: BlockNumber,
    pub consensus_last_block_number: BlockNumber,
    pub consensus_gap_resets: u64,
    pub consensus_recovered_blocks: u64,
    pub consensus_finalized_slot: Slot,
    pub consensus_finalized_number: BlockNumber,

    // Peer metrics
    pub el_connections_ingress: u64,
    pub el_connections_egress: u64,
    pub cl_connections_ingress: u64,
    pub cl_connections_egress: u64,
    pub el_unique_peers_ingress: u64,
    pub el_unique_peers_egress: u64,
    pub cl_unique_peers_ingress: u64,
    pub cl_unique_peers_egress: u64,
    pub el_active_connections_ingress: u64,
    pub el_active_connections_egress: u64,
    pub cl_active_connections_ingress: u64,
    pub cl_active_connections_egress: u64,

    // Message counts
    pub el_tx_hashes_received: u64,
    pub el_txs_received: u64,
    pub cl_blocks_received: u64,
    pub cl_finality_updates_received: u64,
    pub cl_status_received: u64,
    pub cl_status_sent: u64,
    pub cl_blocks_by_range_requests_sent: u64,
    pub cl_blocks_by_range_responses_received: u64,

    // Latency (nanos)
    pub cl_blocks_by_range_latency_avg_ns: u64,
    pub cl_status_latency_avg_ns: u64,
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
    consensus_finalized_slot: AtomicU64,
    consensus_finalized_number: AtomicU64,

    // Peer metrics
    el_connections_ingress: AtomicU64,
    el_connections_egress: AtomicU64,
    cl_connections_ingress: AtomicU64,
    cl_connections_egress: AtomicU64,
    el_active_connections_ingress: AtomicUsize,
    el_active_connections_egress: AtomicUsize,
    cl_active_connections_ingress: AtomicUsize,
    cl_active_connections_egress: AtomicUsize,
    el_unique_peers_ingress: Mutex<HashSet<String>>,
    el_unique_peers_egress: Mutex<HashSet<String>>,
    cl_unique_peers_ingress: Mutex<HashSet<String>>,
    cl_unique_peers_egress: Mutex<HashSet<String>>,

    // Message counts
    el_tx_hashes_received: AtomicU64,
    el_txs_received: AtomicU64,
    cl_blocks_received: AtomicU64,
    cl_finality_updates_received: AtomicU64,
    cl_status_received: AtomicU64,
    cl_status_sent: AtomicU64,
    cl_blocks_by_range_requests_sent: AtomicU64,
    cl_blocks_by_range_responses_received: AtomicU64,

    // Latency tracking
    cl_blocks_by_range_latency_sum: AtomicU64,
    cl_blocks_by_range_latency_count: AtomicU64,
    cl_status_latency_sum: AtomicU64,
    cl_status_latency_count: AtomicU64,
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
            consensus_finalized_slot: AtomicU64::default(),
            consensus_finalized_number: AtomicU64::default(),

            el_connections_ingress: AtomicU64::default(),
            el_connections_egress: AtomicU64::default(),
            cl_connections_ingress: AtomicU64::default(),
            cl_connections_egress: AtomicU64::default(),
            el_active_connections_ingress: AtomicUsize::default(),
            el_active_connections_egress: AtomicUsize::default(),
            cl_active_connections_ingress: AtomicUsize::default(),
            cl_active_connections_egress: AtomicUsize::default(),
            el_unique_peers_ingress: Mutex::new(HashSet::new()),
            el_unique_peers_egress: Mutex::new(HashSet::new()),
            cl_unique_peers_ingress: Mutex::new(HashSet::new()),
            cl_unique_peers_egress: Mutex::new(HashSet::new()),

            el_tx_hashes_received: AtomicU64::default(),
            el_txs_received: AtomicU64::default(),
            cl_blocks_received: AtomicU64::default(),
            cl_finality_updates_received: AtomicU64::default(),
            cl_status_received: AtomicU64::default(),
            cl_status_sent: AtomicU64::default(),
            cl_blocks_by_range_requests_sent: AtomicU64::default(),
            cl_blocks_by_range_responses_received: AtomicU64::default(),

            cl_blocks_by_range_latency_sum: AtomicU64::default(),
            cl_blocks_by_range_latency_count: AtomicU64::default(),
            cl_status_latency_sum: AtomicU64::default(),
            cl_status_latency_count: AtomicU64::default(),
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
    pub(crate) fn record_consensus_anchor(&self, block_number: BlockNumber) {
        self.consensus_anchor_block_number
            .store(block_number.0, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub(crate) fn record_consensus_next_expected(&self, block_number: BlockNumber) {
        self.consensus_next_expected_block_number
            .store(block_number.0, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub(crate) fn record_consensus_last_block(&self, block_number: BlockNumber) {
        self.consensus_last_block_number
            .store(block_number.0, Ordering::Relaxed);
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

    #[allow(dead_code)]
    pub(crate) fn record_consensus_finalized_slot(&self, slot: Slot) {
        self.consensus_finalized_slot.store(slot.0, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub(crate) fn record_consensus_finalized_number(&self, number: BlockNumber) {
        self.consensus_finalized_number.store(number.0, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub(crate) fn record_el_connection(&self, peer_id: String, ingress: bool) {
        if ingress {
            self.el_connections_ingress.fetch_add(1, Ordering::Relaxed);
            self.el_unique_peers_ingress.lock().unwrap().insert(peer_id);
            self.el_active_connections_ingress.fetch_add(1, Ordering::Relaxed);
        } else {
            self.el_connections_egress.fetch_add(1, Ordering::Relaxed);
            self.el_unique_peers_egress.lock().unwrap().insert(peer_id);
            self.el_active_connections_egress.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[allow(dead_code)]
    pub(crate) fn record_el_disconnection(&self, ingress: bool) {
        if ingress {
            self.el_active_connections_ingress.fetch_sub(1, Ordering::Relaxed);
        } else {
            self.el_active_connections_egress.fetch_sub(1, Ordering::Relaxed);
        }
    }

    #[allow(dead_code)]
    pub(crate) fn record_cl_connection(&self, peer_id: String, ingress: bool) {
        if ingress {
            self.cl_connections_ingress.fetch_add(1, Ordering::Relaxed);
            self.cl_unique_peers_ingress.lock().unwrap().insert(peer_id);
            self.cl_active_connections_ingress.fetch_add(1, Ordering::Relaxed);
        } else {
            self.cl_connections_egress.fetch_add(1, Ordering::Relaxed);
            self.cl_unique_peers_egress.lock().unwrap().insert(peer_id);
            self.cl_active_connections_egress.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[allow(dead_code)]
    pub(crate) fn record_cl_disconnection(&self, ingress: bool) {
        if ingress {
            self.cl_active_connections_ingress.fetch_sub(1, Ordering::Relaxed);
        } else {
            self.cl_active_connections_egress.fetch_sub(1, Ordering::Relaxed);
        }
    }

    #[allow(dead_code)]
    pub(crate) fn record_el_tx_hashes_received(&self) {
        self.el_tx_hashes_received.fetch_add(1, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub(crate) fn record_el_txs_received(&self) {
        self.el_txs_received.fetch_add(1, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub(crate) fn record_cl_block_received(&self) {
        self.cl_blocks_received.fetch_add(1, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub(crate) fn record_cl_finality_update_received(&self) {
        self.cl_finality_updates_received.fetch_add(1, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub(crate) fn record_cl_status_received(&self, latency_ns: u64) {
        self.cl_status_received.fetch_add(1, Ordering::Relaxed);
        if latency_ns > 0 {
            self.cl_status_latency_sum.fetch_add(latency_ns, Ordering::Relaxed);
            self.cl_status_latency_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[allow(dead_code)]
    pub(crate) fn record_cl_status_sent(&self) {
        self.cl_status_sent.fetch_add(1, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub(crate) fn record_cl_blocks_by_range_request_sent(&self) {
        self.cl_blocks_by_range_requests_sent.fetch_add(1, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub(crate) fn record_cl_blocks_by_range_response_received(&self, latency_ns: u64) {
        self.cl_blocks_by_range_responses_received.fetch_add(1, Ordering::Relaxed);
        self.cl_blocks_by_range_latency_sum.fetch_add(latency_ns, Ordering::Relaxed);
        self.cl_blocks_by_range_latency_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Exports current snapshots to global metrics (e.g. for Prometheus).
    pub(crate) fn update_prometheus(&self) {
        gauge!("mempool_pending_seen").set(self.pending_seen.load(Ordering::Relaxed) as f64);
        gauge!("mempool_block_count").set(self.block_count.load(Ordering::Relaxed) as f64);
        gauge!("mempool_last_block_txs").set(self.last_block_txs.load(Ordering::Relaxed) as f64);
        gauge!("mempool_last_block_gas_used").set(self.last_block_gas_used.load(Ordering::Relaxed) as f64);

        gauge!("p2p_peer_count_el").set(self.p2p_peer_count.load(Ordering::Relaxed) as f64);
        gauge!("p2p_peer_count_cl").set(self.consensus_peer_count.load(Ordering::Relaxed) as f64);

        gauge!("p2p_connections_ingress_el").set(self.el_connections_ingress.load(Ordering::Relaxed) as f64);
        gauge!("p2p_connections_egress_el").set(self.el_connections_egress.load(Ordering::Relaxed) as f64);
        gauge!("p2p_connections_ingress_cl").set(self.cl_connections_ingress.load(Ordering::Relaxed) as f64);
        gauge!("p2p_connections_egress_cl").set(self.cl_connections_egress.load(Ordering::Relaxed) as f64);

        gauge!("p2p_active_connections_ingress_el").set(self.el_active_connections_ingress.load(Ordering::Relaxed) as f64);
        gauge!("p2p_active_connections_egress_el").set(self.el_active_connections_egress.load(Ordering::Relaxed) as f64);
        gauge!("p2p_active_connections_ingress_cl").set(self.cl_active_connections_ingress.load(Ordering::Relaxed) as f64);
        gauge!("p2p_active_connections_egress_cl").set(self.cl_active_connections_egress.load(Ordering::Relaxed) as f64);

        gauge!("p2p_unique_peers_ingress_el").set(self.el_unique_peers_ingress.lock().unwrap().len() as f64);
        gauge!("p2p_unique_peers_egress_el").set(self.el_unique_peers_egress.lock().unwrap().len() as f64);
        gauge!("p2p_unique_peers_ingress_cl").set(self.cl_unique_peers_ingress.lock().unwrap().len() as f64);
        gauge!("p2p_unique_peers_egress_cl").set(self.cl_unique_peers_egress.lock().unwrap().len() as f64);

        counter!("el_tx_hashes_received_total").absolute(self.el_tx_hashes_received.load(Ordering::Relaxed));
        counter!("el_txs_received_total").absolute(self.el_txs_received.load(Ordering::Relaxed));
        counter!("cl_blocks_received_total").absolute(self.cl_blocks_received.load(Ordering::Relaxed));
        counter!("cl_finality_updates_received_total").absolute(self.cl_finality_updates_received.load(Ordering::Relaxed));
        counter!("cl_status_received_total").absolute(self.cl_status_received.load(Ordering::Relaxed));
        counter!("cl_status_sent_total").absolute(self.cl_status_sent.load(Ordering::Relaxed));
        counter!("cl_blocks_by_range_requests_total").absolute(self.cl_blocks_by_range_requests_sent.load(Ordering::Relaxed));
        counter!("cl_blocks_by_range_responses_total").absolute(self.cl_blocks_by_range_responses_received.load(Ordering::Relaxed));

        let head = self.consensus_last_block_number.load(Ordering::Relaxed);
        let finalized = self.consensus_finalized_number.load(Ordering::Relaxed);
        if head > 0 && finalized > 0 {
            gauge!("chain_unfinalized_depth").set(head.saturating_sub(finalized) as f64);
        }
    }
}
