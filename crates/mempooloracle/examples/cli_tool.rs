use mempooloracle::{
    Address, BlockUpdate, MempoolEvent, MempoolHandle, MempoolTracker, PendingTx, TrackerConfig,
    TxId,
};
use std::{
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

fn main() {
    let (handle, event_sender) = {
        let (event_sender, event_receiver) = mpsc::channel();
        let config = TrackerConfig {
            initial_base_fee: 10,
            global_capacity: 1000,
            per_account_capacity: 100,
            private_flow_prior: 0.1, // Start with a 10% private flow estimate
        };
        let (handle, tracker) = MempoolTracker::new(event_receiver, &[], config);
        thread::spawn(move || tracker.run());
        (handle, event_sender)
    };

    // Spawn a thread to simulate blockchain events
    let event_sender_clone = event_sender.clone();
    thread::spawn(move || simulate_blockchain(event_sender_clone));

    // Main loop to display stats every 2 seconds
    display_stats(handle);
}

fn simulate_blockchain(events: mpsc::Sender<MempoolEvent>) {
    let mut nonce = 0;
    let sender = Address([1; 20]);
    let mut block_number = 0;

    loop {
        // Send a burst of transactions
        for _ in 0..10 {
            let tx = PendingTx {
                id: TxId([nonce as u8; 32]),
                sender,
                nonce,
                max_fee_per_gas: 15 + (nonce % 10) as u128,
                max_priority_fee_per_gas: 1 + (nonce % 5) as u128,
                gas_limit: 21000,
            };
            events.send(MempoolEvent::PendingTransaction(tx)).unwrap();
            nonce += 1;
        }

        thread::sleep(Duration::from_secs(5));

        // Create a new block that includes some of the transactions
        let block_tx_count = 5;
        let included_txs: Vec<PendingTx> = (0..block_tx_count)
            .map(|i| {
                let tx_nonce = nonce - 10 + i;
                PendingTx {
                    id: TxId([tx_nonce as u8; 32]),
                    sender,
                    nonce: tx_nonce,
                    max_fee_per_gas: 15 + (tx_nonce % 10) as u128,
                    max_priority_fee_per_gas: 1 + (tx_nonce % 5) as u128,
                    gas_limit: 21000,
                }
            })
            .collect();

        let block = BlockUpdate {
            included_txs: included_txs.clone(),
            new_base_fee: 10 + (block_number % 5),
            gas_used: 21000 * block_tx_count,
            gas_limit: 30_000_000,
        };

        println!(
            "
--- New Block {} ({} txs) ---",
            block_number,
            block.included_txs.len()
        );

        events.send(MempoolEvent::NewBlock(block)).unwrap();
        block_number += 1;
    }
}

fn display_stats(handle: MempoolHandle) {
    let mut last_run = Instant::now();
    loop {
        if last_run.elapsed() >= Duration::from_secs(2) {
            let base_fee = handle.current_base_fee();
            let private_flow = handle.private_flow_ratio();
            let last_block_gas_limit = handle.last_block_gas_limit();
            let usable_capacity = (last_block_gas_limit as f64 * (1.0 - private_flow)) as u64;

            let mut expected_txs = 0;
            let mut min_tip = 0;
            let mut gas_used = 0;

            let pq = handle.priority_queue();

            for (fee, addr, nonce) in pq.iter().rev() {
                // Iterate in reverse for high-to-low priority
                if let Some(tx) = handle.find_tx_by_addr_and_nonce(*addr, *nonce) {
                    if gas_used + tx.gas_limit > usable_capacity {
                        break;
                    }
                    gas_used += tx.gas_limit;
                    min_tip = *fee;
                    expected_txs += 1;
                }
            }

            println!(
                "Current Base Fee: {} | Min Tip (next block): {} | Expected Txs (next block): {}",
                base_fee, min_tip, expected_txs
            );

            last_run = Instant::now();
        }
        thread::sleep(Duration::from_millis(100));
    }
}
