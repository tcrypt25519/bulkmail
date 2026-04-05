use mempooloracle::{
    Address, BlockUpdate, MempoolEvent, MempoolHandle, MempoolTracker, PendingTx, TrackerConfig,
    TxId,
};
use std::{
    env, process,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

fn main() {
    let duration_secs = parse_duration_secs();
    let stop = Arc::new(AtomicBool::new(false));

    let (handle, event_sender, tracker_thread) = {
        let (event_sender, event_receiver) = mpsc::channel();
        let config = TrackerConfig {
            initial_base_fee: 10,
            global_capacity: 1000,
            per_account_capacity: 100,
            private_flow_prior: 0.1,
        };
        let (handle, tracker) = MempoolTracker::new(event_receiver, &[], config);
        let tracker_thread = thread::spawn(move || tracker.run());
        (handle, event_sender, tracker_thread)
    };

    let simulation_stop = stop.clone();
    let event_sender_clone = event_sender.clone();
    let simulation_thread =
        thread::spawn(move || simulate_blockchain(event_sender_clone, simulation_stop));

    if duration_secs == 0 {
        display_stats(handle, None, stop);
    } else {
        display_stats(
            handle,
            Some(Instant::now() + Duration::from_secs(duration_secs)),
            stop.clone(),
        );
        stop.store(true, Ordering::Relaxed);
        drop(event_sender);
        let _ = simulation_thread.join();
        let _ = tracker_thread.join();
    }
}

fn parse_duration_secs() -> u64 {
    let mut args = env::args().skip(1);
    let mut duration_secs = 30_u64;

    while let Some(arg) = args.next() {
        if let Some(value) = arg.strip_prefix("--duration=") {
            duration_secs = parse_duration_value(value);
            continue;
        }

        if arg == "--duration" {
            let Some(value) = args.next() else {
                eprintln!("missing value for --duration");
                print_usage_and_exit();
            };
            duration_secs = parse_duration_value(&value);
            continue;
        }

        eprintln!("unrecognized argument: {arg}");
        print_usage_and_exit();
    }

    duration_secs
}

fn parse_duration_value(value: &str) -> u64 {
    match value.parse::<u64>() {
        Ok(duration_secs) => duration_secs,
        Err(_) => {
            eprintln!("invalid duration value: {value}");
            print_usage_and_exit();
        }
    }
}

fn print_usage_and_exit() -> ! {
    eprintln!("usage: cargo run -p mempooloracle --example cli_tool -- [--duration <seconds>]");
    eprintln!("default: --duration 30");
    eprintln!("use --duration 0 to run until interrupted");
    process::exit(2);
}

fn simulate_blockchain(events: mpsc::Sender<MempoolEvent>, stop: Arc<AtomicBool>) {
    let mut nonce = 0;
    let sender = Address([1; 20]);
    let mut block_number = 0;

    while !stop.load(Ordering::Relaxed) {
        for _ in 0..10 {
            if stop.load(Ordering::Relaxed) {
                return;
            }

            let tx = PendingTx {
                id: TxId([nonce as u8; 32]),
                sender,
                nonce,
                max_fee_per_gas: 15 + (nonce % 10) as u128,
                max_priority_fee_per_gas: 1 + (nonce % 5) as u128,
                gas_limit: 21000,
            };
            if events.send(MempoolEvent::PendingTransaction(tx)).is_err() {
                return;
            }
            nonce += 1;
        }

        if should_stop_for(&stop, Duration::from_secs(5)) {
            return;
        }

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

        if events.send(MempoolEvent::NewBlock(block)).is_err() {
            return;
        }
        block_number += 1;
    }
}

fn display_stats(handle: MempoolHandle, deadline: Option<Instant>, stop: Arc<AtomicBool>) {
    let mut last_run = Instant::now();

    loop {
        if let Some(deadline) = deadline
            && Instant::now() >= deadline
        {
            return;
        }

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

        if stop.load(Ordering::Relaxed) {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn should_stop_for(stop: &AtomicBool, duration: Duration) -> bool {
    let started = Instant::now();

    while started.elapsed() < duration {
        if stop.load(Ordering::Relaxed) {
            return true;
        }

        let remaining = duration.saturating_sub(started.elapsed());
        thread::sleep(remaining.min(Duration::from_millis(100)));
    }

    stop.load(Ordering::Relaxed)
}
