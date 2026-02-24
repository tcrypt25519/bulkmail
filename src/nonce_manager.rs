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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::MockChainClient;
    use alloy::primitives::Address;

    // Build a mock that always returns the given nonce for get_account_nonce.
    fn mock_at_nonce(initial: u64) -> MockChainClient {
        let mut mock = MockChainClient::new();
        mock.expect_get_account_nonce()
            .returning(move |_| Ok(initial));
        mock
    }

    async fn nonce_manager_at(nonce: u64) -> NonceManager {
        NonceManager::new(Arc::new(mock_at_nonce(nonce)), Address::default())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn test_new_fetches_initial_nonce() {
        let nm = nonce_manager_at(7).await;
        assert_eq!(*nm.current_nonce.lock().await, 7);
    }

    #[tokio::test]
    async fn test_sequential_assignment() {
        let nm = nonce_manager_at(0).await;
        assert_eq!(nm.get_next_available_nonce().await, 0);
        assert_eq!(nm.get_next_available_nonce().await, 1);
        assert_eq!(nm.get_next_available_nonce().await, 2);
    }

    #[tokio::test]
    async fn test_starts_from_offset() {
        // Initial on-chain nonce of 5; first local assignment must be 5.
        let nm = nonce_manager_at(5).await;
        assert_eq!(nm.get_next_available_nonce().await, 5);
        assert_eq!(nm.get_next_available_nonce().await, 6);
    }

    #[tokio::test]
    async fn test_mark_available_allows_reuse() {
        let nm = nonce_manager_at(0).await;
        let n = nm.get_next_available_nonce().await; // assigns 0
        nm.mark_nonce_available(n).await;            // returns 0 to the pool
        // next assignment should give 0 again
        assert_eq!(nm.get_next_available_nonce().await, 0);
    }

    #[tokio::test]
    async fn test_update_advances_baseline_and_prunes_in_flight() {
        let nm = nonce_manager_at(0).await;
        nm.get_next_available_nonce().await; // 0 in-flight
        nm.get_next_available_nonce().await; // 1 in-flight
        // Confirming nonce 2 should prune both 0 and 1.
        nm.update_current_nonce(2).await;
        assert_eq!(*nm.current_nonce.lock().await, 2);
        assert!(nm.in_flight_nonces.lock().await.is_empty());
    }

    #[tokio::test]
    async fn test_update_ignores_rollback() {
        let nm = nonce_manager_at(10).await;
        // A lower value must not roll back the baseline.
        nm.update_current_nonce(5).await;
        assert_eq!(*nm.current_nonce.lock().await, 10);
    }

    #[tokio::test]
    async fn test_update_retains_higher_in_flight_nonces() {
        let nm = nonce_manager_at(0).await;
        nm.get_next_available_nonce().await; // 0
        nm.get_next_available_nonce().await; // 1
        nm.get_next_available_nonce().await; // 2
        // Confirming up to 1 should prune 0 but keep 1 and 2.
        nm.update_current_nonce(1).await;
        let in_flight = nm.in_flight_nonces.lock().await.clone();
        assert!(!in_flight.contains(&0), "nonce 0 is below the new baseline");
        assert!(in_flight.contains(&1), "nonce 1 is at the new baseline");
        assert!(in_flight.contains(&2), "nonce 2 is above the new baseline");
    }

    #[tokio::test]
    async fn test_sync_nonce_updates_from_chain() {
        use std::sync::atomic::{AtomicU32, Ordering};
        // First call (from new()) returns 0; subsequent calls (from sync_nonce) return 5.
        let counter = Arc::new(AtomicU32::new(0));
        let counter2 = counter.clone();
        let mut mock = MockChainClient::new();
        mock.expect_get_account_nonce().returning(move |_| {
            let n = counter2.fetch_add(1, Ordering::SeqCst);
            let nonce = if n == 0 { 0u64 } else { 5u64 };
            Ok(nonce)
        });

        let nm = NonceManager::new(Arc::new(mock), Address::default()).await.unwrap();
        assert_eq!(*nm.current_nonce.lock().await, 0);
        nm.sync_nonce().await.unwrap();
        assert_eq!(*nm.current_nonce.lock().await, 5);
    }
}
