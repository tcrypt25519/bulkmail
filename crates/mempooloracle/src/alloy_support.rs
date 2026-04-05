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
use std::{collections::HashMap, sync::mpsc, thread};
use thiserror::Error;
use tokio::{sync::watch, task::JoinHandle};

#[derive(Debug, Error)]
pub enum AlloyTrackerError {
    #[error("alloy transport error: {0}")]
    Transport(#[from] TransportError),
}

pub struct AlloyTrackerRuntime {
    handle: MempoolHandle,
    shutdown: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
}

impl AlloyTrackerRuntime {
    pub fn handle(&self) -> MempoolHandle {
        self.handle.clone()
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
    let provider = builder.connect_ws(ws).await?.erased();
    connect_erased_provider(provider, config).await
}

pub async fn connect_with_provider<P>(
    provider: P,
    config: TrackerConfig,
) -> Result<AlloyTrackerRuntime, AlloyTrackerError>
where
    P: Provider<Ethereum> + 'static,
{
    connect_erased_provider(provider.erased(), config).await
}

async fn connect_erased_provider(
    provider: DynProvider,
    config: TrackerConfig,
) -> Result<AlloyTrackerRuntime, AlloyTrackerError> {
    let initial_txs = backfill_mempool(&provider).await?;

    let (event_tx, event_rx) = mpsc::channel();
    let (handle, tracker) = MempoolTracker::from_channel(event_rx, &initial_txs, config);
    thread::spawn(move || tracker.run());

    let block_sub = provider.subscribe_blocks().await?;
    let pending_sub = provider.subscribe_full_pending_transactions().await?;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let block_task = tokio::spawn(run_block_subscription(
        provider.clone(),
        event_tx.clone(),
        shutdown_rx.clone(),
        block_sub,
    ));
    let pending_task = tokio::spawn(run_pending_subscription(event_tx, shutdown_rx, pending_sub));

    Ok(AlloyTrackerRuntime {
        handle,
        shutdown: shutdown_tx,
        tasks: vec![block_task, pending_task],
    })
}

async fn backfill_mempool(provider: &DynProvider) -> Result<Vec<PendingTx>, AlloyTrackerError> {
    let txpool: TxpoolContents = match provider
        .raw_request("txpool_contents".into(), NoParams::default())
        .await
    {
        Ok(contents) => contents,
        Err(_) => {
            provider
                .raw_request("txpool_content".into(), NoParams::default())
                .await?
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

async fn run_block_subscription(
    provider: DynProvider,
    event_tx: mpsc::Sender<MempoolEvent>,
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

                if event_tx.send(MempoolEvent::NewBlock(block_update)).is_err() {
                    break;
                }
            }
        }
    }
}

async fn run_pending_subscription(
    event_tx: mpsc::Sender<MempoolEvent>,
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
