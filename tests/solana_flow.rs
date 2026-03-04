#![cfg(feature = "solana")]

use async_trait::async_trait;
use bulkmail::{
    Error, Message, Sender,
    adapter::{
        BlockReceiver, ChainAdapter, ChainClient, FeeManager, ReplayProtection, RetryDecision,
        RetryStrategy, SendOutcome, TransactionStatus,
    },
};
use solana_sdk::{hash::Hash, instruction::Instruction, pubkey::Pubkey, signature::Signature};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use tokio::{
    sync::mpsc,
    time::{Duration, timeout},
};

// -----------------------------------------------------------------------------
// Adapter: Solana message flow
// -----------------------------------------------------------------------------
#[derive(Debug)]
struct SolAdapterTest;

#[derive(Debug, Clone)]
struct SolClientTest {
    receiver: Arc<Mutex<Option<BlockReceiver>>>,
    saw_payload: Arc<AtomicBool>,
}

#[async_trait]
impl ChainClient<SolAdapterTest> for SolClientTest {
    async fn subscribe_new_blocks(&self) -> Result<BlockReceiver, Error> {
        self.receiver
            .lock()
            .expect("receiver lock poisoned")
            .take()
            .ok_or(Error::SubscriptionClosed)
    }

    async fn get_block_number(&self) -> Result<u64, Error> {
        Ok(0)
    }

    async fn send_transaction(
        &self,
        msg: &Message,
        _fee: &u64,
        _replay_token: &Hash,
    ) -> Result<SendOutcome<SolAdapterTest>, Error> {
        let Some(payload) = msg.solana.as_ref() else {
            return Err(Error::SolanaError("missing solana payload".to_string()));
        };
        assert_eq!(payload.instructions.len(), 1);
        self.saw_payload.store(true, Ordering::Relaxed);
        Ok(SendOutcome::Confirmed {
            tx_id: Signature::default(),
        })
    }

    async fn get_transaction_status(&self, _id: &Signature) -> Result<TransactionStatus, Error> {
        Ok(TransactionStatus::Confirmed { number: 1 })
    }
}

#[derive(Debug, Default)]
struct SolFeeTest;

#[async_trait]
impl FeeManager<SolAdapterTest> for SolFeeTest {
    async fn get_fee_params(&self, _priority: u32) -> Result<u64, Error> {
        Ok(1)
    }

    async fn update_on_confirmation(&self, _confirmation_time: Duration, _fee_paid: &u64) {}

    fn bump_fee(&self, current: &u64) -> u64 {
        current.saturating_add(1)
    }

    async fn get_base_fee(&self) -> u64 {
        1
    }
}

#[derive(Debug, Default)]
struct SolReplayTest {
    released: AtomicBool,
}

#[async_trait]
impl ReplayProtection<SolAdapterTest> for SolReplayTest {
    async fn next(&self) -> Hash {
        Hash::new_unique()
    }

    async fn sync(&self) -> Result<(), Error> {
        Ok(())
    }

    async fn release(&self, _token: &Hash) {
        self.released.store(true, Ordering::Relaxed);
    }
}

#[derive(Debug, Default)]
struct SolRetryTest;

#[async_trait]
impl RetryStrategy<SolAdapterTest> for SolRetryTest {
    async fn handle_dropped(
        &self,
        _pending: &bulkmail::adapter::PendingTransaction<SolAdapterTest>,
        _client: &SolClientTest,
        _fees: &SolFeeTest,
        _replay: &SolReplayTest,
    ) -> RetryDecision<SolAdapterTest> {
        RetryDecision::Abandon
    }

    async fn handle_confirmed(
        &self,
        _pending: &bulkmail::adapter::PendingTransaction<SolAdapterTest>,
        _fees: &SolFeeTest,
        _replay: &SolReplayTest,
        _confirmation_time: Duration,
    ) {
    }
}

impl ChainAdapter for SolAdapterTest {
    type FeeParams = u64;
    type ReplayToken = Hash;
    type TxId = Signature;
    type Client = SolClientTest;
    type FeeManager = SolFeeTest;
    type ReplayProtection = SolReplayTest;
    type RetryStrategy = SolRetryTest;
}

#[tokio::test]
async fn sender_processes_solana_message() {
    let (tx, rx) = mpsc::channel::<u64>(1);
    let saw_payload = Arc::new(AtomicBool::new(false));

    let sender = Sender::<SolAdapterTest>::new(
        Arc::new(SolClientTest {
            receiver: Arc::new(Mutex::new(Some(rx))),
            saw_payload: saw_payload.clone(),
        }),
        Arc::new(SolFeeTest),
        Arc::new(SolReplayTest::default()),
        Arc::new(SolRetryTest),
    );

    let instruction = Instruction {
        program_id: Pubkey::new_unique(),
        accounts: Vec::new(),
        data: vec![1, 2, 3],
    };

    sender
        .add_message(Message::solana(vec![instruction], 5, None, 0))
        .await;

    let run_sender = sender.clone();
    let run_handle = tokio::spawn(async move { run_sender.run().await });

    tx.send(1).await.expect("block send failed");

    timeout(Duration::from_secs(2), async {
        while !saw_payload.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("solana payload was not processed");

    drop(tx);

    let result = timeout(Duration::from_secs(2), run_handle)
        .await
        .expect("sender run timed out")
        .expect("sender task panicked");

    assert!(matches!(result, Err(Error::SubscriptionClosed)));
}
