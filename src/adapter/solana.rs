//! Solana adapter — partial Solana support behind the `solana` feature.
//!
//! [`Sol`] is the zero-sized tag type. Instantiate a [`Sender<Sol>`] once the
//! Solana adapter is implemented.
//!
//! [`Sender<Sol>`]: crate::Sender

use crate::{
    Error,
    adapter::{
        BlockReceiver, ChainAdapter, ChainClient, FeeManager, PendingTransaction, ReplayProtection,
        RetryDecision, RetryStrategy, SendOutcome, TransactionStatus,
    },
};
use async_trait::async_trait;
use log::warn;
use solana_client::{nonblocking::rpc_client::RpcClient, pubsub_client::PubsubClient};
use solana_sdk::{
    commitment_config::CommitmentConfig,
    compute_budget::ComputeBudgetInstruction,
    hash::Hash,
    message::Message as SolanaMessage,
    signature::{Keypair, Signature, Signer},
    transaction::{TransactionError, VersionedTransaction},
};
use solana_transaction_status::TransactionConfirmationStatus;
use std::cmp::min;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// Solana fee parameters (placeholder).
#[derive(Debug, Clone)]
pub struct SolFeeParams {
    /// Compute unit price in micro-lamports.
    pub compute_unit_price: u64,
    /// Compute unit limit.
    pub compute_unit_limit: u32,
}

impl Default for SolFeeParams {
    fn default() -> Self {
        Self {
            compute_unit_price: DEFAULT_COMPUTE_UNIT_PRICE,
            compute_unit_limit: DEFAULT_COMPUTE_UNIT_LIMIT,
        }
    }
}

/// Solana adapter tag type. All SOL-specific types are associated here.
pub struct Sol;

impl ChainAdapter for Sol {
    type FeeParams = SolFeeParams;
    type ReplayToken = Hash; // recent blockhash
    type TxId = Signature; // transaction signature
    type Client = SolClient;
    type FeeManager = SolFeeManager;
    type ReplayProtection = SolReplayProtection;
    type RetryStrategy = SolRetryStrategy;
}

/// Solana chain client (RPC + Pubsub).
pub struct SolClient {
    rpc: Arc<RpcClient>,
    pubsub_url: String,
    payer: Arc<Keypair>,
}

impl SolClient {
    #[allow(dead_code)]
    pub fn new(
        rpc: Arc<RpcClient>,
        pubsub_url: impl Into<String>,
        payer: Arc<Keypair>,
    ) -> Self {
        Self {
            rpc,
            pubsub_url: pubsub_url.into(),
            payer,
        }
    }
}

#[async_trait]
impl ChainClient<Sol> for SolClient {
    async fn subscribe_new_blocks(&self) -> Result<BlockReceiver, Error> {
        let (subscription, receiver) = PubsubClient::slot_subscribe(&self.pubsub_url)
            .map_err(|err| Error::SolanaError(format!("slot_subscribe failed: {err}")))?;

        let (tx, rx) = tokio::sync::mpsc::channel(32);
        std::thread::spawn(move || {
            // Keep the subscription alive for the lifetime of this thread.
            let _subscription = subscription;
            for slot_info in receiver.iter() {
                if tx.blocking_send(slot_info.slot).is_err() {
                    break;
                }
            }
        });

        Ok(rx)
    }

    async fn get_block_number(&self) -> Result<u64, Error> {
        self.rpc
            .get_slot()
            .await
            .map_err(|err| Error::SolanaError(format!("get_slot failed: {err}")))
    }

    async fn send_transaction(
        &self,
        msg: &crate::Message,
        fee: &SolFeeParams,
        replay_token: &Hash,
    ) -> Result<SendOutcome<Sol>, Error> {
        let payload = msg
            .solana
            .as_ref()
            .ok_or_else(|| Error::SolanaError("missing solana payload".to_string()))?;

        let mut instructions = Vec::with_capacity(payload.instructions.len() + 2);
        if fee.compute_unit_limit > 0 {
            instructions.push(ComputeBudgetInstruction::set_compute_unit_limit(
                fee.compute_unit_limit,
            ));
        }
        if fee.compute_unit_price > 0 {
            instructions.push(ComputeBudgetInstruction::set_compute_unit_price(
                fee.compute_unit_price,
            ));
        }
        instructions.extend(payload.instructions.iter().cloned());

        let payer = self.payer.pubkey();
        let message = SolanaMessage::new_with_blockhash(&instructions, Some(&payer), replay_token);
        let tx = VersionedTransaction::try_new(message, &[self.payer.as_ref()]).map_err(|err| {
            Error::SolanaError(format!("failed to sign transaction: {err}"))
        })?;

        let signature = self
            .rpc
            .send_transaction(&tx)
            .await
            .map_err(|err| Error::SolanaError(format!("send_transaction failed: {err}")))?;

        let start = std::time::Instant::now();
        loop {
            let status = self.get_transaction_status(&signature).await?;
            match status {
                TransactionStatus::Confirmed { .. } | TransactionStatus::Finalized { .. } => {
                    return Ok(SendOutcome::Confirmed { tx_id: signature })
                }
                TransactionStatus::Failed { .. } => {
                    return Ok(SendOutcome::Reverted { tx_id: signature })
                }
                TransactionStatus::Expired => {
                    return Ok(SendOutcome::Dropped { tx_id: signature })
                }
                TransactionStatus::Pending => {
                    if start.elapsed() >= Duration::from_secs(SOLANA_TX_TIMEOUT_SECS) {
                        return Ok(SendOutcome::Dropped { tx_id: signature });
                    }
                    tokio::time::sleep(Duration::from_millis(SOLANA_STATUS_POLL_MS)).await;
                }
            }
        }
    }

    async fn get_transaction_status(&self, _id: &Signature) -> Result<TransactionStatus, Error> {
        let status = self
            .rpc
            .get_signature_statuses(&[*_id])
            .await
            .map_err(|err| Error::SolanaError(format!("get_signature_statuses failed: {err}")))?
            .value
            .into_iter()
            .next()
            .flatten();

        let status = match status {
            None => return Ok(TransactionStatus::Pending),
            Some(status) => status,
        };

        if let Some(err) = status.err {
            if matches!(err, TransactionError::BlockhashNotFound) {
                return Ok(TransactionStatus::Expired);
            }
            return Ok(TransactionStatus::Failed {
                reason: format!("{err:?}"),
            });
        }

        match status.confirmation_status() {
            TransactionConfirmationStatus::Finalized => Ok(TransactionStatus::Finalized {
                number: status.slot,
            }),
            TransactionConfirmationStatus::Confirmed | TransactionConfirmationStatus::Processed => {
                Ok(TransactionStatus::Confirmed {
                    number: status.slot,
                })
            }
        }
    }
}

/// Solana fee manager (placeholder).
pub struct SolFeeManager;

#[async_trait]
impl FeeManager<Sol> for SolFeeManager {
    async fn get_fee_params(&self, priority: u32) -> Result<SolFeeParams, Error> {
        let priority = min(priority, crate::message::MAX_PRIORITY) as u64;
        let price_range = MAX_COMPUTE_UNIT_PRICE.saturating_sub(DEFAULT_COMPUTE_UNIT_PRICE);
        let price = DEFAULT_COMPUTE_UNIT_PRICE
            .saturating_add(price_range.saturating_mul(priority) / crate::message::MAX_PRIORITY as u64);

        Ok(SolFeeParams {
            compute_unit_price: price,
            compute_unit_limit: DEFAULT_COMPUTE_UNIT_LIMIT,
        })
    }

    async fn update_on_confirmation(&self, _confirmation_time: Duration, _fee_paid: &SolFeeParams) {
        // Placeholder for future fee model updates.
    }

    fn bump_fee(&self, current: &SolFeeParams) -> SolFeeParams {
        SolFeeParams {
            compute_unit_price: bump_by_percent(current.compute_unit_price, FEE_BUMP_PERCENT),
            compute_unit_limit: current.compute_unit_limit,
        }
    }

    async fn get_base_fee(&self) -> SolFeeParams {
        Ok(SolFeeParams::default())
    }
}

/// Solana replay protection (blockhash cache placeholder).
pub struct SolReplayProtection {
    rpc: Arc<RpcClient>,
    state: Mutex<BlockhashState>,
}

#[derive(Clone, Copy, Debug)]
struct BlockhashState {
    hash: Hash,
    last_valid_block_height: u64,
}

const BLOCKHASH_REFRESH_MARGIN: u64 = 30;

impl SolReplayProtection {
    pub async fn new(rpc: Arc<RpcClient>) -> Result<Self, Error> {
        let (hash, last_valid_block_height) = rpc
            .get_latest_blockhash_with_commitment(CommitmentConfig::processed())
            .await
            .map_err(|err| Error::SolanaError(format!("get_latest_blockhash failed: {err}")))?;

        Ok(Self {
            rpc,
            state: Mutex::new(BlockhashState {
                hash,
                last_valid_block_height,
            }),
        })
    }
}

#[async_trait]
impl ReplayProtection<Sol> for SolReplayProtection {
    async fn next(&self) -> Hash {
        let state = self.state.lock().await;
        state.hash
    }

    async fn sync(&self) -> Result<(), Error> {
        let current_height = self
            .rpc
            .get_block_height()
            .await
            .map_err(|err| Error::SolanaError(format!("get_block_height failed: {err}")))?;

        let should_refresh = {
            let state = self.state.lock().await;
            current_height + BLOCKHASH_REFRESH_MARGIN >= state.last_valid_block_height
        };

        if should_refresh {
            let (hash, last_valid_block_height) = self
                .rpc
                .get_latest_blockhash_with_commitment(CommitmentConfig::processed())
                .await
                .map_err(|err| Error::SolanaError(format!("get_latest_blockhash failed: {err}")))?;

            let mut state = self.state.lock().await;
            state.hash = hash;
            state.last_valid_block_height = last_valid_block_height;
        }

        Ok(())
    }
}

/// Solana retry strategy (placeholder).
pub struct SolRetryStrategy;

#[async_trait]
impl RetryStrategy<Sol> for SolRetryStrategy {
    async fn handle_dropped(
        &self,
        _pending: &PendingTransaction<Sol>,
        _client: &SolClient,
        _fees: &SolFeeManager,
        _replay: &SolReplayProtection,
    ) -> RetryDecision<Sol> {
        if let Err(err) = _replay.sync().await {
            warn!("failed to refresh blockhash after drop: {err:?}");
            return RetryDecision::Requeue;
        }

        let new_fee = _fees.bump_fee(&_pending.fee);
        let new_replay = _replay.next().await;
        RetryDecision::Resubmit {
            fee: new_fee,
            replay_token: new_replay,
        }
    }

    async fn handle_confirmed(
        &self,
        _pending: &PendingTransaction<Sol>,
        _fees: &SolFeeManager,
        _replay: &SolReplayProtection,
        _confirmation_time: Duration,
    ) {
        _fees
            .update_on_confirmation(_confirmation_time, &_pending.fee)
            .await;
        if let Err(err) = _replay.sync().await {
            warn!("failed to refresh blockhash after confirmation: {err:?}");
        }
    }
}

const DEFAULT_COMPUTE_UNIT_LIMIT: u32 = 200_000;
const DEFAULT_COMPUTE_UNIT_PRICE: u64 = 0;
const MAX_COMPUTE_UNIT_PRICE: u64 = 10_000;
const FEE_BUMP_PERCENT: u64 = 20;
const SOLANA_TX_TIMEOUT_SECS: u64 = 30;
const SOLANA_STATUS_POLL_MS: u64 = 500;

fn bump_by_percent(value: u64, percent: u64) -> u64 {
    let bump = value.saturating_mul(percent) / 100;
    value.saturating_add(bump)
}
