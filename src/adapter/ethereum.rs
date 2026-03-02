//! Ethereum adapter — wraps existing chain, gas-price, and nonce modules.
//!
//! [`Eth`] is the zero-sized tag type. Instantiate a [`Sender<Eth>`] to send
//! EIP-1559 transactions.
//!
//! [`Sender<Eth>`]: crate::Sender

use crate::adapter::{
    BlockReceiver, ChainAdapter, ChainClient, FeeManager, PendingTransaction, ReplayProtection,
    RetryDecision, RetryStrategy, SendOutcome, TransactionStatus,
};
use crate::{chain, Error, GasPriceManager, NonceManager};
use alloy::consensus::TxEip1559;
use alloy::primitives::{Address, TxKind, B256};
use async_trait::async_trait;
use log::{error, info, warn};
use std::sync::Arc;
use std::time::Duration;

/// Maximum number of times a stuck transaction may be replaced with a higher fee.
const MAX_REPLACEMENTS: u32 = 3;

/// Percentage by which the priority fee is increased on each replacement.
const GAS_PRICE_INCREASE_PERCENT: u8 = 20;

/// Seconds to wait for a transaction to confirm before treating it as dropped.
const TX_TIMEOUT: u64 = 3;

// ── Ethereum types ──────────────────────────────────────────────────────────

/// EIP-1559 fee parameters: (base_fee, priority_fee) in wei.
#[derive(Debug, Clone)]
pub struct EthFeeParams {
    pub base_fee: u128,
    pub priority_fee: u128,
}

/// Ethereum adapter tag type. All ETH-specific types are associated here.
pub struct Eth;

impl ChainAdapter for Eth {
    type FeeParams = EthFeeParams;
    type ReplayToken = u64; // nonce
    type TxId = B256; // tx hash
    type Client = EthClient;
    type FeeManager = EthFeeManager;
    type ReplayProtection = EthReplayProtection;
    type RetryStrategy = EthRetryStrategy;
}

// ── EthClient ───────────────────────────────────────────────────────────────

/// Wraps the existing [`chain::Chain`] struct as a [`ChainClient<Eth>`].
pub struct EthClient {
    inner: Arc<dyn chain::ChainClient>,
}

impl EthClient {
    pub fn new(chain: Arc<dyn chain::ChainClient>) -> Self {
        Self { inner: chain }
    }

    pub fn chain_id(&self) -> u64 {
        self.inner.id()
    }
}

#[async_trait]
impl ChainClient<Eth> for EthClient {
    async fn subscribe_new_blocks(&self) -> Result<BlockReceiver, Error> {
        let mut header_rx = self.inner.subscribe_new_blocks().await?;
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        tokio::spawn(async move {
            while let Some(header) = header_rx.recv().await {
                if tx.send(header.inner.number).await.is_err() {
                    break;
                }
            }
        });
        Ok(rx)
    }

    async fn get_block_number(&self) -> Result<u64, Error> {
        Ok(self.inner.get_block_number().await?)
    }

    async fn send_transaction(
        &self,
        msg: &crate::Message,
        fee: &EthFeeParams,
        nonce: &u64,
    ) -> Result<SendOutcome<Eth>, Error> {
        let tx = TxEip1559 {
            chain_id: self.inner.id(),
            to: msg.to.map_or(TxKind::Create, TxKind::Call),
            value: msg.value,
            input: msg.data.clone(),
            nonce: *nonce,
            gas_limit: msg.gas,
            max_fee_per_gas: fee.base_fee + fee.priority_fee,
            max_priority_fee_per_gas: fee.priority_fee,
            access_list: Default::default(),
        };

        let watcher = self.inner.send_transaction(tx).await?;
        let tx_hash = *watcher.tx_hash();
        info!("Sent transaction {:?} with nonce {}", tx_hash, nonce);

        // Watch for confirmation with timeout
        let pending = watcher
            .with_required_confirmations(1)
            .with_timeout(Some(Duration::from_secs(TX_TIMEOUT)))
            .register()
            .await?;

        match pending.await {
            Ok(_hash) => {
                // Check receipt for status
                match self.inner.get_receipt(tx_hash).await {
                    Ok(Some(receipt)) if receipt.status() => {
                        Ok(SendOutcome::Confirmed { tx_id: tx_hash })
                    }
                    Ok(Some(_receipt)) => {
                        // Transaction reverted
                        Ok(SendOutcome::Reverted { tx_id: tx_hash })
                    }
                    Ok(None) => {
                        // Timeout / no receipt
                        Ok(SendOutcome::Dropped { tx_id: tx_hash })
                    }
                    Err(e) => {
                        error!("Error getting receipt for {:?}: {}", tx_hash, e);
                        Ok(SendOutcome::Dropped { tx_id: tx_hash })
                    }
                }
            }
            Err(_) => {
                // Dropped / timed out
                Ok(SendOutcome::Dropped { tx_id: tx_hash })
            }
        }
    }

    async fn get_transaction_status(&self, tx_hash: &B256) -> Result<TransactionStatus, Error> {
        match self.inner.get_receipt(*tx_hash).await {
            Ok(Some(receipt)) => {
                if receipt.status() {
                    Ok(TransactionStatus::Confirmed {
                        number: receipt.block_number.unwrap_or(0),
                    })
                } else {
                    Ok(TransactionStatus::Failed {
                        reason: "reverted".to_string(),
                    })
                }
            }
            Ok(None) => Ok(TransactionStatus::Pending),
            Err(e) => Err(Error::ChainError(e)),
        }
    }
}

// ── EthFeeManager ───────────────────────────────────────────────────────────

/// Wraps the existing [`GasPriceManager`] as a [`FeeManager<Eth>`].
pub struct EthFeeManager {
    inner: GasPriceManager,
}

impl EthFeeManager {
    pub fn new() -> Self {
        Self {
            inner: GasPriceManager::new(),
        }
    }
}

impl Default for EthFeeManager {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl FeeManager<Eth> for EthFeeManager {
    async fn get_fee_params(&self, priority: u32) -> Result<EthFeeParams, Error> {
        let (base_fee, priority_fee) = self.inner.get_gas_price(priority)?;
        Ok(EthFeeParams {
            base_fee,
            priority_fee,
        })
    }

    async fn update_on_confirmation(&self, confirmation_time: Duration, fee_paid: &EthFeeParams) {
        self.inner
            .update_on_confirmation(confirmation_time, fee_paid.priority_fee);
    }

    fn bump_fee(&self, current: &EthFeeParams) -> EthFeeParams {
        EthFeeParams {
            base_fee: current.base_fee,
            priority_fee: bump_by_percent(current.priority_fee, GAS_PRICE_INCREASE_PERCENT),
        }
    }

    async fn get_base_fee(&self) -> EthFeeParams {
        let base_fee = self.inner.get_base_fee();
        EthFeeParams {
            base_fee,
            priority_fee: 0,
        }
    }
}

// ── EthReplayProtection ─────────────────────────────────────────────────────

/// Wraps the existing [`NonceManager`] as [`ReplayProtection<Eth>`].
pub struct EthReplayProtection {
    inner: NonceManager,
}

impl EthReplayProtection {
    pub async fn new(chain: Arc<dyn chain::ChainClient>, address: Address) -> Result<Self, Error> {
        Ok(Self {
            inner: NonceManager::new(chain, address).await?,
        })
    }

    /// Returns a nonce to the available pool.
    ///
    /// This is an Ethereum-specific method not required by the
    /// [`ReplayProtection`] trait. It is called by [`EthRetryStrategy`]
    /// when a transaction fails before broadcast.
    pub async fn release_nonce(&self, nonce: u64) {
        self.inner.mark_nonce_available(nonce);
    }

    /// Advances the confirmed nonce baseline.
    ///
    /// This is an Ethereum-specific method not required by the
    /// [`ReplayProtection`] trait. It is called by [`EthRetryStrategy`]
    /// on transaction confirmation.
    pub async fn confirm_nonce(&self, nonce: u64) {
        self.inner.update_current_nonce(nonce);
    }
}

#[async_trait]
impl ReplayProtection<Eth> for EthReplayProtection {
    async fn next(&self) -> u64 {
        self.inner.get_next_available_nonce()
    }

    async fn sync(&self) -> Result<(), Error> {
        self.inner.sync_nonce().await
    }

    async fn release(&self, token: &u64) {
        self.inner.mark_nonce_available(*token);
    }
}

// ── EthRetryStrategy ────────────────────────────────────────────────────────

/// Ethereum retry strategy: replace at the same nonce with bumped priority fee.
pub struct EthRetryStrategy;

impl EthRetryStrategy {
    pub fn new() -> Self {
        Self
    }
}

impl Default for EthRetryStrategy {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RetryStrategy<Eth> for EthRetryStrategy {
    async fn handle_dropped(
        &self,
        pending: &PendingTransaction<Eth>,
        _client: &EthClient,
        fees: &EthFeeManager,
        replay: &EthReplayProtection,
    ) -> RetryDecision<Eth> {
        // Check if we have retries left
        if !pending.msg.can_retry() {
            warn!(
                "Message for transaction {:?} failed after max retries",
                pending.tx_id
            );
            // Release the nonce so it can be reused
            replay.release_nonce(pending.replay_token).await;
            return RetryDecision::Abandon;
        }

        // Check if we've exceeded max replacements
        if pending.replacement_count >= MAX_REPLACEMENTS {
            // Release the nonce and requeue the message
            replay.release_nonce(pending.replay_token).await;
            return RetryDecision::Requeue;
        }

        // Bump the fee and resubmit at the same nonce
        let new_fee = fees.bump_fee(&pending.fee);
        RetryDecision::Resubmit {
            fee: new_fee,
            replay_token: pending.replay_token, // same nonce
        }
    }

    async fn handle_confirmed(
        &self,
        pending: &PendingTransaction<Eth>,
        fees: &EthFeeManager,
        replay: &EthReplayProtection,
        confirmation_time: Duration,
    ) {
        info!("Transaction {:?} confirmed", pending.tx_id);
        futures::future::join(
            fees.update_on_confirmation(confirmation_time, &pending.fee),
            replay.confirm_nonce(pending.replay_token),
        )
        .await;
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

pub(crate) fn bump_by_percent(n: u128, percent: u8) -> u128 {
    n + n * percent as u128 / 100
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bump_zero_base() {
        assert_eq!(bump_by_percent(0, 20), 0);
    }

    #[test]
    fn test_bump_twenty_percent() {
        assert_eq!(bump_by_percent(100, 20), 120);
    }

    #[test]
    fn test_bump_zero_percent() {
        assert_eq!(bump_by_percent(500, 0), 500);
    }

    #[test]
    fn test_bump_is_an_increase_not_a_fraction() {
        let fee: u128 = 1_000_000_000;
        let bumped = bump_by_percent(fee, 20);
        assert!(bumped > fee, "bumped fee must exceed the original");
        assert_eq!(bumped, 1_200_000_000);
    }
}
