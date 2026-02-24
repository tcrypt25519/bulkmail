//! In-flight nonce tracking and per-block synchronization. See [`NonceManager`].

use crate::{chain::ChainClient, Error};
use std::collections::BTreeSet;
use std::sync::Arc;
use tokio::sync::Mutex;
use alloy::primitives::Address;

/// A thread-safe tracker for Ethereum account nonces.
///
/// [`NonceManager`] maintains two pieces of state: the last confirmed nonce
/// fetched from the chain, and the set of nonces currently in-flight.
/// [`get_next_available_nonce`] atomically advances past any in-flight nonces
/// to assign the next unused one. Every new block triggers [`sync_nonce`] to
/// reconcile local state against the on-chain confirmed nonce.
///
/// When a transaction is dropped or fails before broadcast, callers must call
/// [`mark_nonce_available`] to return the nonce to the pool. When a transaction
/// confirms, [`update_current_nonce`] advances the baseline and prunes stale
/// in-flight entries.
///
/// [`get_next_available_nonce`]: Self::get_next_available_nonce
/// [`mark_nonce_available`]: Self::mark_nonce_available
/// [`sync_nonce`]: Self::sync_nonce
/// [`update_current_nonce`]: Self::update_current_nonce
pub(crate) struct NonceManager {
    chain: Arc<dyn ChainClient>,
    address: Address,
    current_nonce: Mutex<u64>,
    in_flight_nonces: Mutex<BTreeSet<u64>>,
}

impl NonceManager {
    /// Creates a new [`NonceManager`] by fetching the current confirmed nonce
    /// for `address` from `chain`.
    pub async fn new(chain: Arc<dyn ChainClient>, address: Address) -> Result<Self, Error> {
        let current_nonce = chain.get_account_nonce(address).await?;

        Ok(Self {
            chain,
            address,
            current_nonce: Mutex::new(current_nonce),
            in_flight_nonces: Mutex::new(BTreeSet::new()),
        })
    }

    /// Assigns and reserves the next unused nonce, skipping any in-flight ones.
    ///
    /// This method holds both locks atomically for the duration of the
    /// assignment to prevent two concurrent tasks from receiving the same nonce.
    pub async fn get_next_available_nonce(&self) -> u64 {
        let mut current_nonce = self.current_nonce.lock().await;
        let mut in_flight_nonces = self.in_flight_nonces.lock().await;

        // Find the first non-used nonce
        while in_flight_nonces.contains(&current_nonce) {
            *current_nonce += 1;
        }

        let nonce = *current_nonce;
        // *current_nonce += 1;
        in_flight_nonces.insert(nonce);

        nonce
    }

    /// Returns `nonce` to the available pool without advancing the baseline.
    ///
    /// This method must be called when a transaction is dropped or fails before
    /// it is broadcast, so the nonce can be reused.
    pub async fn mark_nonce_available(&self, nonce: u64) {
        self.in_flight_nonces.lock().await.remove(&nonce);
    }

    /// Advances the confirmed-nonce baseline to `new_nonce` and prunes
    /// in-flight entries that are now below it.
    ///
    /// This method is a no-op if `new_nonce` is not greater than the current
    /// baseline, preventing rollbacks on out-of-order confirmation callbacks.
    pub async fn update_current_nonce(&self, new_nonce: u64) {
        let mut current_nonce = self.current_nonce.lock().await;
        if new_nonce > *current_nonce {
            *current_nonce = new_nonce;
            // Remove all in-flight nonces less than the new current nonce
            let mut in_flight_nonces = self.in_flight_nonces.lock().await;
            in_flight_nonces.retain(|&n| n >= new_nonce);
        }
    }

    /// Fetches the on-chain confirmed nonce and calls [`update_current_nonce`].
    ///
    /// [`Sender::run`] calls this method on every new block to keep the local
    /// state consistent with the chain, recovering from any gaps caused by
    /// dropped or replaced transactions.
    ///
    /// [`update_current_nonce`]: Self::update_current_nonce
    /// [`Sender::run`]: crate::Sender::run
    pub async fn sync_nonce(&self) -> Result<(), Error> {
        let on_chain_nonce = self.chain.get_account_nonce(self.address).await?;
        self.update_current_nonce(on_chain_nonce).await;
        Ok(())
    }
}
