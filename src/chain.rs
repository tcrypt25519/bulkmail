//! Alloy WebSocket wrapper for Ethereum interaction. See [`Chain`] and [`ChainClient`].

use alloy::{
    consensus::{TxEip1559, TypedTransaction},
    network::{Ethereum, EthereumWallet, NetworkWallet},
    primitives::{Address, B256, BlockNumber},
    providers::{DynProvider, PendingTransactionBuilder, Provider, ProviderBuilder, WsConnect},
    rpc::types::{Header, TransactionReceipt},
    signers::{k256::ecdsa::SigningKey, local::PrivateKeySigner},
    transports::{RpcError, TransportErrorKind},
};
use async_trait::async_trait;
use thiserror::Error;
use tokio::sync::mpsc;

/// An error originating from [`Chain`] operations.
#[derive(Error, Debug)]
pub enum Error {
    /// An RPC transport error.
    #[error("RPC error: {0}")]
    Rpc(#[from] RpcError<TransportErrorKind>),

    /// A WebSocket subscription failure.
    #[error("subscription error: {0}")]
    Subscription(String),

    /// A transaction signing failure.
    #[error("signing error: {0}")]
    Signing(#[from] alloy::signers::Error),
}

/// A channel receiver that yields new block [`Header`] values.
///
/// This is the return type of [`ChainClient::subscribe_new_blocks`]. Using a
/// channel type (rather than Alloy's [`Subscription`]) makes it possible to
/// inject a mock implementation in tests.
///
/// [`Subscription`]: alloy::pubsub::Subscription
pub type BlockReceiver = mpsc::Receiver<Header>;

/// The interface that [`Sender`] and the nonce manager require from the chain layer.
///
/// [`Chain`] implements this trait for production use. Tests can implement it
/// with a mock (a `MockChainClient` is generated automatically under `#[cfg(test)]`
/// via `mockall`).
///
/// [`Sender`]: crate::Sender
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait ChainClient: Send + Sync {
    /// Returns the chain ID.
    fn id(&self) -> u64;

    /// Subscribes to new block headers, returning a [`BlockReceiver`] channel.
    async fn subscribe_new_blocks(&self) -> Result<BlockReceiver, Error>;

    /// Returns the current best block number.
    async fn get_block_number(&self) -> Result<BlockNumber, Error>;

    /// Returns the confirmed transaction count (nonce) for `address`.
    async fn get_account_nonce(&self, address: Address) -> Result<u64, Error>;

    /// Returns the receipt for `tx_hash`, or `None` if not yet mined.
    async fn get_receipt(&self, tx_hash: B256) -> Result<Option<TransactionReceipt>, Error>;

    /// Signs `tx` and broadcasts it, returning a watcher for confirmation.
    async fn send_transaction(
        &self,
        tx: TxEip1559,
    ) -> Result<PendingTransactionBuilder<Ethereum>, Error>;
}

/// An Alloy WebSocket wrapper that signs and submits EIP-1559 transactions.
///
/// [`Chain`] requires a WebSocket URL because it subscribes to new block
/// headers. HTTP providers do not support subscriptions.
///
/// All outgoing transactions are signed with the [`SigningKey`] supplied at
/// construction time. The `chain_id` is embedded in every signed transaction
/// and must match the target network.
#[derive(Clone)]
pub struct Chain {
    chain_id: u64,
    wallet: EthereumWallet,
    provider: DynProvider<Ethereum>,
}

impl Chain {
    /// Creates a new [`Chain`] connected to the given WebSocket `url`.
    ///
    /// `signing_key` signs all outgoing transactions. `chain_id` must match
    /// the network's chain ID or transactions will be rejected.
    pub async fn new(url: &str, signing_key: SigningKey, chain_id: u64) -> Result<Self, Error> {
        let ws = WsConnect::new(url);
        let signer = PrivateKeySigner::from_signing_key(signing_key);
        let provider = ProviderBuilder::default().connect_ws(ws).await?.erased();

        Ok(Self {
            chain_id,
            wallet: EthereumWallet::new(signer),
            provider,
        })
    }

    /// Returns the chain ID this [`Chain`] was constructed with.
    pub fn id(&self) -> u64 {
        self.chain_id
    }

    /// Returns the current best block number.
    pub async fn get_block_number(&self) -> Result<BlockNumber, Error> {
        Ok(self.provider.get_block_number().await?)
    }

    /// Returns the confirmed transaction count (nonce) for `address`.
    pub async fn get_account_nonce(&self, address: Address) -> Result<u64, Error> {
        Ok(self.provider.get_transaction_count(address).await?)
    }

    /// Returns the receipt for `tx_hash`, or `None` if not yet mined.
    pub async fn get_receipt(&self, tx_hash: B256) -> Result<Option<TransactionReceipt>, Error> {
        Ok(self.provider.get_transaction_receipt(tx_hash).await?)
    }

    /// Signs `tx` and broadcasts it, returning a watcher for confirmation.
    pub async fn send_transaction(
        &self,
        tx: TxEip1559,
    ) -> Result<PendingTransactionBuilder<Ethereum>, Error> {
        let envelope = <EthereumWallet as NetworkWallet<Ethereum>>::sign_transaction(
            &self.wallet,
            TypedTransaction::Eip1559(tx),
        )
        .await?;
        let pending = self.provider.send_tx_envelope(envelope).await?;
        Ok(pending)
    }
}

#[async_trait]
impl ChainClient for Chain {
    fn id(&self) -> u64 {
        Chain::id(self)
    }

    /// Wraps the Alloy block subscription in an mpsc channel so that
    /// [`Sender`] can be tested without a live WebSocket.
    ///
    /// [`Sender`]: crate::Sender
    async fn subscribe_new_blocks(&self) -> Result<BlockReceiver, Error> {
        let subscription = self.provider.subscribe_blocks().await?;
        let (tx, rx) = mpsc::channel(32);
        tokio::spawn(async move {
            let mut sub = subscription;
            loop {
                match sub.recv().await {
                    Ok(header) => {
                        if tx.send(header).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(rx)
    }

    async fn get_block_number(&self) -> Result<BlockNumber, Error> {
        Chain::get_block_number(self).await
    }

    async fn get_account_nonce(&self, address: Address) -> Result<u64, Error> {
        Chain::get_account_nonce(self, address).await
    }

    async fn get_receipt(&self, tx_hash: B256) -> Result<Option<TransactionReceipt>, Error> {
        Chain::get_receipt(self, tx_hash).await
    }

    async fn send_transaction(
        &self,
        tx: TxEip1559,
    ) -> Result<PendingTransactionBuilder<Ethereum>, Error> {
        Chain::send_transaction(self, tx).await
    }
}
