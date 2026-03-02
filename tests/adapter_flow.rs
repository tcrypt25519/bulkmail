use async_trait::async_trait;
use bulkmail::adapter::{
    BlockReceiver, ChainAdapter, ChainClient, FeeManager, ReplayProtection, RetryDecision,
    RetryStrategy, SendOutcome, TransactionStatus,
};
use bulkmail::{Error, Message, Sender};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use tokio::sync::mpsc;
use tokio::time::{timeout, Duration};

#[derive(Debug)]
struct DummyAdapter;

#[derive(Debug, Clone)]
struct DummyClient {
    receiver: Arc<Mutex<Option<BlockReceiver>>>,
    send_called: Arc<AtomicBool>,
}

#[async_trait]
impl ChainClient<DummyAdapter> for DummyClient {
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
        _msg: &Message,
        _fee: &u64,
        _replay_token: &u64,
    ) -> Result<SendOutcome<DummyAdapter>, Error> {
        self.send_called.store(true, Ordering::Relaxed);
        Ok(SendOutcome::Confirmed { tx_id: 1 })
    }

    async fn get_transaction_status(&self, _id: &u64) -> Result<TransactionStatus, Error> {
        Ok(TransactionStatus::Confirmed { number: 1 })
    }
}

#[derive(Debug, Default)]
struct DummyFeeManager;

#[async_trait]
impl FeeManager<DummyAdapter> for DummyFeeManager {
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
struct DummyReplay {
    next_value: AtomicU64,
}

#[async_trait]
impl ReplayProtection<DummyAdapter> for DummyReplay {
    async fn next(&self) -> u64 {
        self.next_value.fetch_add(1, Ordering::Relaxed)
    }

    async fn sync(&self) -> Result<(), Error> {
        Ok(())
    }
}

#[derive(Debug, Default)]
struct DummyRetry;

#[async_trait]
impl RetryStrategy<DummyAdapter> for DummyRetry {
    async fn handle_dropped(
        &self,
        _pending: &bulkmail::adapter::PendingTransaction<DummyAdapter>,
        _client: &DummyClient,
        _fees: &DummyFeeManager,
        _replay: &DummyReplay,
    ) -> RetryDecision<DummyAdapter> {
        RetryDecision::Abandon
    }

    async fn handle_confirmed(
        &self,
        _pending: &bulkmail::adapter::PendingTransaction<DummyAdapter>,
        _fees: &DummyFeeManager,
        _replay: &DummyReplay,
        _confirmation_time: Duration,
    ) {
    }
}

impl ChainAdapter for DummyAdapter {
    type FeeParams = u64;
    type ReplayToken = u64;
    type TxId = u64;
    type Client = DummyClient;
    type FeeManager = DummyFeeManager;
    type ReplayProtection = DummyReplay;
    type RetryStrategy = DummyRetry;
}

#[tokio::test]
async fn sender_processes_message_with_adapter() {
    let (tx, rx) = mpsc::channel(1);
    let send_called = Arc::new(AtomicBool::new(false));
    let client = DummyClient {
        receiver: Arc::new(Mutex::new(Some(rx))),
        send_called: send_called.clone(),
    };

    let sender = Sender::<DummyAdapter>::new(
        Arc::new(client),
        Arc::new(DummyFeeManager::default()),
        Arc::new(DummyReplay::default()),
        Arc::new(DummyRetry::default()),
    );

    sender.add_message(Message::default()).await;

    let run_sender = sender.clone();
    let run_handle = tokio::spawn(async move { run_sender.run().await });

    tx.send(1).await.expect("block send failed");

    timeout(Duration::from_secs(2), async {
        while !send_called.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("send_transaction was never called");

    drop(tx);

    let result = timeout(Duration::from_secs(2), run_handle)
        .await
        .expect("sender run timed out")
        .expect("sender task panicked");

    assert!(matches!(result, Err(Error::SubscriptionClosed)));
}
