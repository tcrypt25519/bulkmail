use crate::{
    Address, BlockUpdate, MempoolEvent, MempoolHandle, MempoolTracker, PendingTx, TrackerConfig,
    TxId,
};
use alloy::{
    consensus::{BlockHeader as _, Transaction as _},
    eips::eip1559::BaseFeeParams,
    network::{BlockResponse as _, Ethereum, TransactionResponse as _},
    providers::{
        DynProvider, Provider, ProviderBuilder, ProviderLayer, RootProvider, WsConnect,
        fillers::TxFiller,
    },
    pubsub::Subscription,
    rpc::{client::NoParams, types::Transaction as RpcTransaction},
    transports::TransportError,
};
use futures::StreamExt;
use serde::Deserialize;
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
};
use thiserror::Error;
use tokio::{sync::watch, task::JoinHandle, time::{Duration, timeout}};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const SUBSCRIPTION_TIMEOUT: Duration = Duration::from_secs(10);
const BACKFILL_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Error)]
pub enum AlloyTrackerError {
    #[error("alloy transport error: {0}")]
    Transport(#[from] TransportError),
    #[error("timed out during {stage}")]
    Timeout { stage: &'static str },
}

pub struct AlloyTrackerRuntime {
    handle: MempoolHandle,
    telemetry: Arc<RuntimeTelemetry>,
    shutdown: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
}

impl AlloyTrackerRuntime {
    pub fn handle(&self) -> MempoolHandle {
        self.handle.clone()
    }

    pub fn telemetry(&self) -> AlloyTrackerTelemetry {
        AlloyTrackerTelemetry {
            inner: self.telemetry.clone(),
        }
    }
}

impl Drop for AlloyTrackerRuntime {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        for task in &self.tasks {
            task.abort();
        }
    }
}

pub async fn connect_with_builder<L, F>(
    builder: ProviderBuilder<L, F>,
    ws: WsConnect,
    config: TrackerConfig,
) -> Result<AlloyTrackerRuntime, AlloyTrackerError>
where
    L: ProviderLayer<RootProvider, Ethereum>,
    F: TxFiller<Ethereum> + ProviderLayer<L::Provider, Ethereum>,
    F::Provider: 'static,
{
    let provider = timeout(CONNECT_TIMEOUT, builder.connect_ws(ws))
        .await
        .map_err(|_| AlloyTrackerError::Timeout {
            stage: "websocket connect",
        })??
        .erased();
    connect_erased_provider(provider, config).await
}

pub async fn connect_with_provider<P>(
    provider: P,
    config: TrackerConfig,
) -> Result<AlloyTrackerRuntime, AlloyTrackerError>
where
    P: Provider<Ethereum> + 'static,
{
    install_rustls_provider();
    connect_erased_provider(provider.erased(), config).await
}

#[derive(Clone)]
pub struct AlloyTrackerTelemetry {
    inner: Arc<RuntimeTelemetry>,
}

impl AlloyTrackerTelemetry {
    pub fn snapshot(&self) -> AlloyTrackerTelemetrySnapshot {
        AlloyTrackerTelemetrySnapshot {
            pending_seen: self.inner.pending_seen.load(Ordering::Relaxed),
            block_count: self.inner.block_count.load(Ordering::Relaxed),
            last_block_txs: self.inner.last_block_txs.load(Ordering::Relaxed),
            last_block_gas_used: self.inner.last_block_gas_used.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AlloyTrackerTelemetrySnapshot {
    pub pending_seen: u64,
    pub block_count: u64,
    pub last_block_txs: usize,
    pub last_block_gas_used: u64,
}

#[derive(Default)]
struct RuntimeTelemetry {
    pending_seen: AtomicU64,
    block_count: AtomicU64,
    last_block_txs: AtomicUsize,
    last_block_gas_used: AtomicU64,
}

fn install_rustls_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

async fn connect_erased_provider(
    provider: DynProvider,
    config: TrackerConfig,
) -> Result<AlloyTrackerRuntime, AlloyTrackerError> {
    install_rustls_provider();
    let (event_tx, event_rx) = mpsc::channel();
    let telemetry = Arc::new(RuntimeTelemetry::default());
    let block_sub = timeout(SUBSCRIPTION_TIMEOUT, provider.subscribe_blocks())
        .await
        .map_err(|_| AlloyTrackerError::Timeout {
            stage: "block subscription setup",
        })??;
    let pending_sub = timeout(
        SUBSCRIPTION_TIMEOUT,
        provider.subscribe_full_pending_transactions(),
    )
    .await
    .map_err(|_| AlloyTrackerError::Timeout {
        stage: "pending transaction subscription setup",
    })??;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let block_task = tokio::spawn(run_block_subscription(
        provider.clone(),
        event_tx.clone(),
        telemetry.clone(),
        shutdown_rx.clone(),
        block_sub,
    ));
    let pending_task = tokio::spawn(run_pending_subscription(
        event_tx.clone(),
        telemetry.clone(),
        shutdown_rx.clone(),
        pending_sub,
    ));

    let (handle, tracker) = MempoolTracker::from_channel(event_rx, &[], config);
    thread::spawn(move || tracker.run());
    let backfill_task = tokio::spawn(run_backfill(
        provider,
        event_tx,
        shutdown_rx,
    ));

    Ok(AlloyTrackerRuntime {
        handle,
        telemetry,
        shutdown: shutdown_tx,
        tasks: vec![block_task, pending_task, backfill_task],
    })
}

async fn backfill_mempool(provider: &DynProvider) -> Result<Vec<PendingTx>, AlloyTrackerError> {
    let txpool: TxpoolContents = match timeout(
        BACKFILL_TIMEOUT,
        provider.raw_request("txpool_contents".into(), NoParams::default()),
    )
    .await
    {
        Ok(Ok(contents)) => contents,
        Ok(Err(_)) | Err(_) => {
            timeout(
                BACKFILL_TIMEOUT,
                provider.raw_request("txpool_content".into(), NoParams::default()),
            )
            .await
            .map_err(|_| AlloyTrackerError::Timeout {
                stage: "txpool_content backfill",
            })??
        }
    };

    Ok(txpool
        .pending
        .into_values()
        .chain(txpool.queued.into_values())
        .flat_map(HashMap::into_values)
        .map(alloy_tx_to_pending_tx)
        .collect())
}

async fn run_backfill(
    provider: DynProvider,
    event_tx: mpsc::Sender<MempoolEvent>,
    mut shutdown: watch::Receiver<bool>,
) {
    tokio::select! {
        _ = shutdown.changed() => {}
        result = backfill_mempool(&provider) => {
            let Ok(txs) = result else {
                return;
            };

            for tx in txs {
                if shutdown.has_changed().unwrap_or(false) {
                    break;
                }
                if event_tx.send(MempoolEvent::PendingTransaction(tx)).is_err() {
                    break;
                }
            }
        }
    }
}

async fn run_block_subscription(
    provider: DynProvider,
    event_tx: mpsc::Sender<MempoolEvent>,
    telemetry: Arc<RuntimeTelemetry>,
    mut shutdown: watch::Receiver<bool>,
    block_sub: Subscription<alloy::rpc::types::Header>,
) {
    let mut stream = block_sub.into_stream();

    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            maybe_header = stream.next() => {
                let Some(header) = maybe_header else {
                    break;
                };

                let Ok(Some(block)) = provider.get_block_by_hash(header.hash).full().await else {
                    continue;
                };

                let included_txs = block
                    .transactions()
                    .txns()
                    .cloned()
                    .map(alloy_tx_to_pending_tx)
                    .collect();

                let new_base_fee = block
                    .header()
                    .next_block_base_fee(BaseFeeParams::ethereum())
                    .or_else(|| block.header().base_fee_per_gas())
                    .unwrap_or_default() as u128;

                let block_update = BlockUpdate {
                    included_txs,
                    new_base_fee,
                    gas_used: block.header().gas_used(),
                    gas_limit: block.header().gas_limit(),
                };

                telemetry.block_count.fetch_add(1, Ordering::Relaxed);
                telemetry
                    .last_block_txs
                    .store(block_update.included_txs.len(), Ordering::Relaxed);
                telemetry
                    .last_block_gas_used
                    .store(block_update.gas_used, Ordering::Relaxed);

                if event_tx.send(MempoolEvent::NewBlock(block_update)).is_err() {
                    break;
                }
            }
        }
    }
}

async fn run_pending_subscription(
    event_tx: mpsc::Sender<MempoolEvent>,
    telemetry: Arc<RuntimeTelemetry>,
    mut shutdown: watch::Receiver<bool>,
    pending_sub: Subscription<RpcTransaction>,
) {
    let mut stream = pending_sub.into_stream();

    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            maybe_tx = stream.next() => {
                let Some(tx) = maybe_tx else {
                    break;
                };
                telemetry.pending_seen.fetch_add(1, Ordering::Relaxed);

                if event_tx
                    .send(MempoolEvent::PendingTransaction(alloy_tx_to_pending_tx(tx)))
                    .is_err()
                {
                    break;
                }
            }
        }
    }
}

fn alloy_tx_to_pending_tx(tx: RpcTransaction) -> PendingTx {
    PendingTx {
        id: TxId(tx.tx_hash().into()),
        sender: Address(tx.from().into_array()),
        nonce: tx.nonce(),
        max_fee_per_gas: alloy::consensus::Transaction::max_fee_per_gas(&tx),
        max_priority_fee_per_gas: tx
            .max_priority_fee_per_gas()
            .unwrap_or_else(|| tx.priority_fee_or_price()),
        gas_limit: tx.gas_limit(),
    }
}

#[derive(Debug, Deserialize)]
struct TxpoolContents {
    #[serde(default)]
    pending: HashMap<String, HashMap<String, RpcTransaction>>,
    #[serde(default)]
    queued: HashMap<String, HashMap<String, RpcTransaction>>,
}
