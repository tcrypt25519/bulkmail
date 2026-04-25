//! Comprehensive reorg handling integration tests
#[cfg(test)]
mod tests {
    use crate::{
        Address, BlockNumber, BlockUpdate, ExecutionHash, MempoolInner, PendingTx, TrackerConfig,
    };
    use std::collections::HashMap;

    /// Create a test block with the given parameters
    fn create_test_block(
        number: u64,
        hash: &[u8; 32],
        parent_hash: &[u8; 32],
        tx_count: usize,
    ) -> BlockUpdate {
        BlockUpdate {
            number: BlockNumber(number),
            hash: ExecutionHash::from(*hash),
            parent_hash: ExecutionHash::from(*parent_hash),
            included_txs: (0..tx_count)
                .map(|i| PendingTx {
                    id: ExecutionHash::from([i as u8; 32]),
                    sender: Address([i as u8; 20]),
                    nonce: i as u64,
                    max_fee_per_gas: 1000000000,
                    max_priority_fee_per_gas: 1000000000,
                    gas_limit: 21000,
                    seen_at: std::time::SystemTime::now(),
                })
                .collect(),
            new_base_fee: 1000000000,
            gas_used: (tx_count * 21000) as u64,
            gas_limit: 30000000,
        }
    }

    /// Create a test mempool inner with initial state
    fn create_test_mempool_inner() -> MempoolInner {
        let config = TrackerConfig {
            initial_base_fee: 1000000000,
            global_capacity: 10000,
            per_account_capacity: 100,
            private_flow_prior: 0.1,
        };
        MempoolInner {
            account_queues: HashMap::new(),
            base_fee_eligibility: std::collections::BTreeMap::new(),
            priority_queue: std::collections::BTreeMap::new(),
            current_base_fee: config.initial_base_fee,
            private_flow_ratio: config.private_flow_prior,
            last_block_gas_limit: 30_000_000,
            last_block_number: None,
            last_block_hash: None,
            history: std::collections::VecDeque::with_capacity(32),
            config,
        }
    }

    #[test]
    fn test_simple_reorg_same_parent_different_hash() {
        let mut mempool = create_test_mempool_inner();

        // Build initial chain: Block 0 -> Block 1 -> Block 2
        let block0 = create_test_block(0, &[0; 32], &[0; 32], 0);
        let block1 = create_test_block(1, &[1; 32], &[0; 32], 2);
        let block2 = create_test_block(2, &[2; 32], &[1; 32], 3);

        // Attach blocks to build history
        mempool.attach_block(block0.clone());
        mempool.attach_block(block1.clone());
        mempool.attach_block(block2.clone());

        // Verify state before reorg
        assert_eq!(mempool.last_block_hash, Some(block2.hash));
        assert_eq!(mempool.history.len(), 3);

        // Create fork: Block 0 -> Block 1 -> Block 2' (same parent, different hash)
        let block2_fork = create_test_block(2, &[3; 32], &[1; 32], 1);
        assert_ne!(block2.hash, block2_fork.hash);
        assert_eq!(block2.parent_hash, block2_fork.parent_hash);

        // Trigger reorg - this should detach block 2 and attach block 2'
        println!(
            "Before reorg: history.len() = {}, last_block_hash = {:?}",
            mempool.history.len(),
            mempool.last_block_hash
        );
        mempool.handle_reorg(block2_fork.clone());
        println!(
            "After reorg: history.len() = {}, last_block_hash = {:?}",
            mempool.history.len(),
            mempool.last_block_hash
        );

        // Verify reorg results
        assert_eq!(mempool.last_block_hash, Some(block2_fork.hash));
        assert_eq!(mempool.last_block_number, Some(BlockNumber(2)));
        assert_eq!(mempool.history.len(), 2); // Should have 2 blocks after reorg (0, 1, 2')

        // The latest block in history should be the forked block
        if let Some(latest_entry) = mempool.history.back() {
            assert_eq!(latest_entry.block.hash, block2_fork.hash);
        }
    }

    #[test]
    fn test_reorg_walk_back_multiple_blocks() {
        let mut mempool = create_test_mempool_inner();

        // Build initial chain: Block 0 -> Block 1 -> Block 2 -> Block 3
        let block0 = create_test_block(0, &[0; 32], &[0; 32], 0);
        let block1 = create_test_block(1, &[1; 32], &[0; 32], 2);
        let block2 = create_test_block(2, &[2; 32], &[1; 32], 3);
        let block3 = create_test_block(3, &[3; 32], &[2; 32], 4);

        // Attach blocks to build history
        mempool.attach_block(block0.clone());
        mempool.attach_block(block1.clone());
        mempool.attach_block(block2.clone());
        mempool.attach_block(block3.clone());

        // Verify state before reorg
        assert_eq!(mempool.last_block_hash, Some(block3.hash));
        assert_eq!(mempool.history.len(), 4);

        // Create fork that diverges at block 1: Block 0 -> Block 1 -> Block 3'
        let block3_fork = create_test_block(3, &[5; 32], &[1; 32], 2); // Parent is block1

        // Trigger reorg with block3_fork - should walk back to block1, then attach block3_fork
        println!("Before reorg: history.len() = {}", mempool.history.len());
        mempool.handle_reorg(block3_fork.clone());
        println!("After reorg: history.len() = {}", mempool.history.len());

        // Verify reorg results
        assert_eq!(mempool.last_block_hash, Some(block3_fork.hash));
        assert_eq!(mempool.last_block_number, Some(BlockNumber(3)));

        // History should contain: block0, block1, block3_fork (block2 and block3 were detached, block3_fork attached)
        assert_eq!(mempool.history.len(), 2);

        let history_blocks: Vec<_> = mempool.history.iter().map(|e| &e.block.hash).collect();
        assert_eq!(history_blocks[0], &block0.hash);
        assert_eq!(history_blocks[1], &block3_fork.hash);
    }

    #[test]
    fn test_reorg_transaction_restoration() {
        let mut mempool = create_test_mempool_inner();

        // Create some test transactions
        let tx1 = PendingTx {
            id: ExecutionHash::from([1; 32]),
            sender: Address([1; 20]),
            nonce: 0,
            max_fee_per_gas: 2000000000,
            max_priority_fee_per_gas: 1000000000,
            gas_limit: 21000,
            seen_at: std::time::SystemTime::now(),
        };
        let tx2 = PendingTx {
            id: ExecutionHash::from([2; 32]),
            sender: Address([2; 20]),
            nonce: 0,
            max_fee_per_gas: 2000000000,
            max_priority_fee_per_gas: 1000000000,
            gas_limit: 21000,
            seen_at: std::time::SystemTime::now(),
        };

        // Insert transactions into mempool
        mempool.insert(tx1.clone());
        mempool.insert(tx2.clone());

        // Build initial chain with tx1 included, tx2 not included
        let block0 = create_test_block(0, &[0; 32], &[0; 32], 0);
        let block1 = create_test_block(1, &[1; 32], &[0; 32], 1);
        let block1_with_tx = BlockUpdate {
            included_txs: vec![tx1.clone()],
            ..block1
        };

        // Attach blocks
        mempool.attach_block(block0);
        mempool.attach_block(block1_with_tx.clone());

        // tx1 should be removed (included in block), tx2 should still be in mempool
        assert!(!mempool.contains_tx(&tx1.id));
        assert!(mempool.contains_tx(&tx2.id));

        // Create fork without tx1 included
        let block1_fork = create_test_block(1, &[2; 32], &[0; 32], 0); // No transactions

        // Trigger reorg - should restore tx1 to mempool
        mempool.handle_reorg(block1_fork.clone());

        // Both transactions should now be back in mempool
        assert!(mempool.contains_tx(&tx1.id));
        assert!(mempool.contains_tx(&tx2.id));
    }

    #[test]
    fn test_deep_reorg_fallback_to_prune() {
        let mut mempool = create_test_mempool_inner();

        // Fill the 32-block history buffer
        for i in 0..32 {
            let parent_hash = if i == 0 { [0; 32] } else { [(i - 1) as u8; 32] };
            let block = create_test_block(i, &[i as u8; 32], &parent_hash, 1);
            mempool.attach_block(block);
        }

        // Verify history is full
        assert_eq!(mempool.history.len(), 32);
        assert_eq!(mempool.last_block_number, Some(BlockNumber(31)));

        // Create a reorg that diverges from a block outside our history (block 0)
        let deep_fork_block = create_test_block(32, &[100; 32], &[0; 32], 1); // Claims parent is block 0

        // This should trigger fallback to prune_and_reanchor since block 0 is not in history
        mempool.handle_reorg(deep_fork_block.clone());

        // After fallback, history should be reset and only contain the new block
        assert_eq!(mempool.history.len(), 1);
        assert_eq!(mempool.last_block_hash, Some(deep_fork_block.hash));
        assert_eq!(mempool.last_block_number, Some(BlockNumber(32)));
    }

    #[test]
    fn test_reorg_no_common_ancestor_in_history() {
        let mut mempool = create_test_mempool_inner();

        // Build a short chain: Block 10 -> Block 11 -> Block 12
        let block10 = create_test_block(10, &[10; 32], &[9; 32], 0);
        let block11 = create_test_block(11, &[11; 32], &[10; 32], 1);
        let block12 = create_test_block(12, &[12; 32], &[11; 32], 1);

        mempool.attach_block(block10.clone());
        mempool.attach_block(block11.clone());
        mempool.attach_block(block12.clone());

        // Create a block that claims parent is block 5 (not in our history)
        let orphan_block = create_test_block(13, &[13; 32], &[5; 32], 1);

        // This should trigger fallback since block 5 is not in history
        mempool.handle_reorg(orphan_block.clone());

        // Should fallback to prune_and_reanchor
        assert_eq!(mempool.history.len(), 1);
        assert_eq!(mempool.last_block_hash, Some(orphan_block.hash));
    }

    #[test]
    fn test_reorg_empty_history() {
        let mut mempool = create_test_mempool_inner();

        // No blocks in history
        assert_eq!(mempool.history.len(), 0);
        assert_eq!(mempool.last_block_hash, None);

        // Create a block and trigger reorg
        let new_block = create_test_block(1, &[1; 32], &[0; 32], 1);

        // Should handle gracefully and attach the block
        mempool.handle_reorg(new_block.clone());

        assert_eq!(mempool.history.len(), 1);
        assert_eq!(mempool.last_block_hash, Some(new_block.hash));
    }

    // Helper method to check if transaction exists in mempool
    trait MempoolTestExt {
        fn contains_tx(&self, tx_id: &ExecutionHash) -> bool;
    }

    impl MempoolTestExt for MempoolInner {
        fn contains_tx(&self, tx_id: &ExecutionHash) -> bool {
            for account_queue in self.account_queues.values() {
                for slot in &account_queue.slots {
                    if let Some(tx) = slot {
                        if tx.id == *tx_id {
                            return true;
                        }
                    }
                }
            }
            false
        }
    }
}
