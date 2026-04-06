#[cfg(test)]
mod tests {
    use mempooloracle::*;
    use std::sync::mpsc;

    fn new_tracker(
        initial_txs: &[PendingTx],
        config: TrackerConfig,
    ) -> (MempoolHandle, mpsc::Sender<MempoolEvent>) {
        let (tx, rx) = mpsc::channel();
        let (handle, tracker) = MempoolTracker::new(rx, initial_txs, config);
        std::thread::spawn(move || tracker.run());
        (handle, tx)
    }

    fn default_config() -> TrackerConfig {
        TrackerConfig {
            initial_base_fee: 10,
            global_capacity: 100,
            per_account_capacity: 10,
            private_flow_prior: 0.0,
        }
    }

    #[tokio::test]
    async fn test_p2p_transport_requires_feature() {
        let result = MempoolTracker::connect(
            TrackerTransport::P2p(P2pTransportConfig {
                chain: "mainnet".to_owned(),
                bootnodes: vec![],
                discovery_v4: true,
                listen_addr: None,
                block_transport: P2pBlockTransport::Disabled,
            }),
            default_config(),
        )
        .await;

        assert!(result.is_err(), "p2p transport should be gated");
        let err = result.err().unwrap();

        assert!(matches!(
            err,
            TrackerError::FeatureDisabled("reth-p2p") | TrackerError::UnsupportedTransport(_)
        ));
    }

    #[test]
    fn test_classification_marketable() {
        let tx = PendingTx {
            id: TxId([1; 32]),
            sender: Address([1; 20]),
            nonce: 0,
            max_fee_per_gas: 20,
            max_priority_fee_per_gas: 10,
            gas_limit: 21000,
        };
        let (handle, _sender) = new_tracker(&[tx.clone()], default_config());

        let classification = handle.classification(&tx.id);
        assert_eq!(classification, Some(TxClassification::Marketable));
    }

    #[test]
    fn test_classification_base_fee_invalid() {
        let tx = PendingTx {
            id: TxId([1; 32]),
            sender: Address([1; 20]),
            nonce: 0,
            max_fee_per_gas: 5,
            max_priority_fee_per_gas: 10,
            gas_limit: 21000,
        };
        let (handle, _sender) = new_tracker(&[tx.clone()], default_config());

        let classification = handle.classification(&tx.id);
        assert_eq!(classification, Some(TxClassification::BaseFeeInvalid));
    }

    #[test]
    fn test_classification_unmarketable() {
        let tx = PendingTx {
            id: TxId([1; 32]),
            sender: Address([1; 20]),
            nonce: 0,
            max_fee_per_gas: 10,
            max_priority_fee_per_gas: 0,
            gas_limit: 21000,
        };
        let (handle, _sender) = new_tracker(&[tx.clone()], default_config());

        let classification = handle.classification(&tx.id);
        assert_eq!(classification, Some(TxClassification::Unmarketable));
    }

    #[test]
    fn test_classification_nonce_bound_gap() {
        let tx1 = PendingTx {
            id: TxId([1; 32]),
            sender: Address([1; 20]),
            nonce: 0,
            max_fee_per_gas: 20,
            max_priority_fee_per_gas: 10,
            gas_limit: 21000,
        };
        let tx2 = PendingTx {
            id: TxId([2; 32]),
            sender: Address([1; 20]),
            nonce: 2, // Gap at nonce 1
            max_fee_per_gas: 20,
            max_priority_fee_per_gas: 10,
            gas_limit: 21000,
        };
        let (handle, sender) = new_tracker(&[], default_config());
        sender.send(MempoolEvent::PendingTransaction(tx1)).unwrap();
        sender
            .send(MempoolEvent::PendingTransaction(tx2.clone()))
            .unwrap();

        // Give the tracker a moment to process
        std::thread::sleep(std::time::Duration::from_millis(50));

        let classification = handle.classification(&tx2.id);
        assert_eq!(classification, Some(TxClassification::NonceBound));
    }

    #[test]
    fn test_classification_nonce_bound_unmarketable() {
        let tx1 = PendingTx {
            id: TxId([1; 32]),
            sender: Address([1; 20]),
            nonce: 0,
            max_fee_per_gas: 10,
            max_priority_fee_per_gas: 0, // Unmarketable
            gas_limit: 21000,
        };
        let tx2 = PendingTx {
            id: TxId([2; 32]),
            sender: Address([1; 20]),
            nonce: 1,
            max_fee_per_gas: 20,
            max_priority_fee_per_gas: 10,
            gas_limit: 21000,
        };
        let (handle, sender) = new_tracker(&[], default_config());
        sender.send(MempoolEvent::PendingTransaction(tx1)).unwrap();
        sender
            .send(MempoolEvent::PendingTransaction(tx2.clone()))
            .unwrap();

        std::thread::sleep(std::time::Duration::from_millis(50));

        let classification = handle.classification(&tx2.id);
        assert_eq!(classification, Some(TxClassification::NonceBound));
    }

    #[test]
    fn test_global_eviction() {
        let mut config = default_config();
        config.global_capacity = 1;
        let (handle, sender) = new_tracker(&[], config);

        let tx1 = PendingTx {
            id: TxId([1; 32]),
            sender: Address([1; 20]),
            nonce: 0,
            max_fee_per_gas: 20,
            max_priority_fee_per_gas: 1,
            gas_limit: 21000,
        };
        sender
            .send(MempoolEvent::PendingTransaction(tx1.clone()))
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(handle.classification(&tx1.id).is_some());

        // This one has a lower fee, should be dropped
        let tx2 = PendingTx {
            id: TxId([2; 32]),
            sender: Address([2; 20]),
            nonce: 0,
            max_fee_per_gas: 20,
            max_priority_fee_per_gas: 0,
            gas_limit: 21000,
        };
        sender
            .send(MempoolEvent::PendingTransaction(tx2.clone()))
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(handle.classification(&tx1.id).is_some());
        assert!(handle.classification(&tx2.id).is_none());

        // This one has a higher fee, should evict tx1
        let tx3 = PendingTx {
            id: TxId([3; 32]),
            sender: Address([3; 20]),
            nonce: 0,
            max_fee_per_gas: 20,
            max_priority_fee_per_gas: 2,
            gas_limit: 21000,
        };
        sender
            .send(MempoolEvent::PendingTransaction(tx3.clone()))
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(handle.classification(&tx1.id).is_none());
        assert!(handle.classification(&tx3.id).is_some());
    }

    #[test]
    fn test_out_of_order_nonce_insert_from_backfill() {
        let sender_addr = Address([7; 20]);
        let tx_high = PendingTx {
            id: TxId([5; 32]),
            sender: sender_addr,
            nonce: 5,
            max_fee_per_gas: 20,
            max_priority_fee_per_gas: 10,
            gas_limit: 21_000,
        };
        let tx_low = PendingTx {
            id: TxId([4; 32]),
            sender: sender_addr,
            nonce: 4,
            max_fee_per_gas: 20,
            max_priority_fee_per_gas: 10,
            gas_limit: 21_000,
        };

        let (handle, sender) = new_tracker(&[], default_config());
        sender
            .send(MempoolEvent::PendingTransaction(tx_high.clone()))
            .unwrap();
        sender
            .send(MempoolEvent::PendingTransaction(tx_low.clone()))
            .unwrap();

        std::thread::sleep(std::time::Duration::from_millis(50));

        assert_eq!(
            handle.classification(&tx_low.id),
            Some(TxClassification::Marketable)
        );
        assert_eq!(
            handle.classification(&tx_high.id),
            Some(TxClassification::Marketable)
        );
        assert!(handle.find_tx_by_addr_and_nonce(sender_addr, 4).is_some());
        assert!(handle.find_tx_by_addr_and_nonce(sender_addr, 5).is_some());
    }
}
