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

// -----------------------------------------------------------------------------
// Adapter: Happy path
// -----------------------------------------------------------------------------
#[derive(Debug)]
struct AdapterHappy;

#[derive(Debug, Clone)]
struct ClientHappy {
    receiver: Arc<Mutex<Option<BlockReceiver>>>,
    send_called: Arc<AtomicBool>,
}

#[async_trait]
impl ChainClient<AdapterHappy> for ClientHappy {
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
    ) -> Result<SendOutcome<AdapterHappy>, Error> {
        self.send_called.store(true, Ordering::Relaxed);
        Ok(SendOutcome::Confirmed { tx_id: 1 })
    }

    async fn get_transaction_status(&self, _id: &u64) -> Result<TransactionStatus, Error> {
        Ok(TransactionStatus::Confirmed { number: 1 })
    }
}

#[derive(Debug, Default)]
struct FeeHappy;

#[async_trait]
impl FeeManager<AdapterHappy> for FeeHappy {
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

#[derive(Debug)]
struct ReplayHappy {
    next_value: AtomicU64,
    released: AtomicBool,
}

impl ReplayHappy {
    fn new(start: u64) -> Self {
        Self {
            next_value: AtomicU64::new(start),
            released: AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl ReplayProtection<AdapterHappy> for ReplayHappy {
    async fn next(&self) -> u64 {
        self.next_value.fetch_add(1, Ordering::Relaxed)
    }

    async fn sync(&self) -> Result<(), Error> {
        Ok(())
    }

    async fn release(&self, _token: &u64) {
        self.released.store(true, Ordering::Relaxed);
    }
}

#[derive(Debug, Default)]
struct RetryHappy;

#[async_trait]
impl RetryStrategy<AdapterHappy> for RetryHappy {
    async fn handle_dropped(
        &self,
        _pending: &bulkmail::adapter::PendingTransaction<AdapterHappy>,
        _client: &ClientHappy,
        _fees: &FeeHappy,
        _replay: &ReplayHappy,
    ) -> RetryDecision<AdapterHappy> {
        RetryDecision::Abandon
    }

    async fn handle_confirmed(
        &self,
        _pending: &bulkmail::adapter::PendingTransaction<AdapterHappy>,
        _fees: &FeeHappy,
        _replay: &ReplayHappy,
        _confirmation_time: Duration,
    ) {
    }
}

impl ChainAdapter for AdapterHappy {
    type FeeParams = u64;
    type ReplayToken = u64;
    type TxId = u64;
    type Client = ClientHappy;
    type FeeManager = FeeHappy;
    type ReplayProtection = ReplayHappy;
    type RetryStrategy = RetryHappy;
}

#[tokio::test]
async fn sender_processes_message_with_adapter() {
    let (tx, rx) = mpsc::channel::<u64>(1);
    let send_called = Arc::new(AtomicBool::new(false));
    let client = ClientHappy {
        receiver: Arc::new(Mutex::new(Some(rx))),
        send_called: send_called.clone(),
    };

    let sender = Sender::<AdapterHappy>::new(
        Arc::new(client),
        Arc::new(FeeHappy),
        Arc::new(ReplayHappy::new(0)),
        Arc::new(RetryHappy),
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

// -----------------------------------------------------------------------------
// Adapter: Fee error releases token
// -----------------------------------------------------------------------------
#[derive(Debug)]
struct AdapterFeeErr;

#[derive(Debug, Default)]
struct FeeFail;

#[async_trait]
impl FeeManager<AdapterFeeErr> for FeeFail {
    async fn get_fee_params(&self, _priority: u32) -> Result<u64, Error> {
        Err(Error::GasPriceError("fail".into()))
    }

    async fn update_on_confirmation(&self, _confirmation_time: Duration, _fee_paid: &u64) {}

    fn bump_fee(&self, current: &u64) -> u64 {
        current.saturating_add(1)
    }

    async fn get_base_fee(&self) -> u64 {
        1
    }
}

#[derive(Debug, Clone)]
struct ClientNoop {
    receiver: Arc<Mutex<Option<BlockReceiver>>>,
}

#[async_trait]
impl ChainClient<AdapterFeeErr> for ClientNoop {
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
    ) -> Result<SendOutcome<AdapterFeeErr>, Error> {
        Ok(SendOutcome::Confirmed { tx_id: 1 })
    }

    async fn get_transaction_status(&self, _id: &u64) -> Result<TransactionStatus, Error> {
        Ok(TransactionStatus::Confirmed { number: 1 })
    }
}

#[derive(Debug)]
struct ReplayFeeErr {
    released: AtomicBool,
    next_value: AtomicU64,
}

impl ReplayFeeErr {
    fn new() -> Self {
        Self {
            released: AtomicBool::new(false),
            next_value: AtomicU64::new(0),
        }
    }
}

#[async_trait]
impl ReplayProtection<AdapterFeeErr> for ReplayFeeErr {
    async fn next(&self) -> u64 {
        self.next_value.fetch_add(1, Ordering::Relaxed)
    }

    async fn sync(&self) -> Result<(), Error> {
        Ok(())
    }

    async fn release(&self, _token: &u64) {
        self.released.store(true, Ordering::Relaxed);
    }
}

#[derive(Debug, Default)]
struct RetryNoop;

#[async_trait]
impl RetryStrategy<AdapterFeeErr> for RetryNoop {
    async fn handle_dropped(
        &self,
        _pending: &bulkmail::adapter::PendingTransaction<AdapterFeeErr>,
        _client: &ClientNoop,
        _fees: &FeeFail,
        _replay: &ReplayFeeErr,
    ) -> RetryDecision<AdapterFeeErr> {
        RetryDecision::Abandon
    }

    async fn handle_confirmed(
        &self,
        _pending: &bulkmail::adapter::PendingTransaction<AdapterFeeErr>,
        _fees: &FeeFail,
        _replay: &ReplayFeeErr,
        _confirmation_time: Duration,
    ) {
    }
}

impl ChainAdapter for AdapterFeeErr {
    type FeeParams = u64;
    type ReplayToken = u64;
    type TxId = u64;
    type Client = ClientNoop;
    type FeeManager = FeeFail;
    type ReplayProtection = ReplayFeeErr;
    type RetryStrategy = RetryNoop;
}

#[tokio::test]
async fn releases_replay_token_on_fee_error() {
    let (_tx, rx) = mpsc::channel::<u64>(1);
    let replay = Arc::new(ReplayFeeErr::new());

    let sender = Sender::<AdapterFeeErr>::new(
        Arc::new(ClientNoop {
            receiver: Arc::new(Mutex::new(Some(rx))),
        }),
        Arc::new(FeeFail),
        replay.clone(),
        Arc::new(RetryNoop),
    );

    sender.add_message(Message::default()).await;

    let run_sender = sender.clone();
    let run_handle = tokio::spawn(async move { run_sender.run().await });

    // Allow task to process fee error and release token
    timeout(Duration::from_secs(2), async {
        while !replay.released.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("replay token not released on fee error");

    // Stop run loop
    run_handle.abort();
}

// -----------------------------------------------------------------------------
// Adapter: Resubmit keeps replay token
// -----------------------------------------------------------------------------
#[derive(Debug)]
struct AdapterResubmit;

#[derive(Debug, Default)]
struct FeeResubmit;

#[async_trait]
impl FeeManager<AdapterResubmit> for FeeResubmit {
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

#[derive(Debug, Clone)]
struct ClientFlaky {
    receiver: Arc<Mutex<Option<BlockReceiver>>>,
    attempts: Arc<AtomicU64>,
}

#[async_trait]
impl ChainClient<AdapterResubmit> for ClientFlaky {
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
    ) -> Result<SendOutcome<AdapterResubmit>, Error> {
        let attempt = self.attempts.fetch_add(1, Ordering::Relaxed);
        if attempt == 0 {
            Ok(SendOutcome::Dropped { tx_id: 1 })
        } else {
            Ok(SendOutcome::Confirmed { tx_id: 1 })
        }
    }

    async fn get_transaction_status(&self, _id: &u64) -> Result<TransactionStatus, Error> {
        Ok(TransactionStatus::Confirmed { number: 1 })
    }
}

#[derive(Debug)]
struct ReplayResubmit {
    released: AtomicBool,
    next_value: AtomicU64,
}

impl ReplayResubmit {
    fn new() -> Self {
        Self {
            released: AtomicBool::new(false),
            next_value: AtomicU64::new(100),
        }
    }
}

#[async_trait]
impl ReplayProtection<AdapterResubmit> for ReplayResubmit {
    async fn next(&self) -> u64 {
        self.next_value.fetch_add(1, Ordering::Relaxed)
    }

    async fn sync(&self) -> Result<(), Error> {
        Ok(())
    }

    async fn release(&self, _token: &u64) {
        self.released.store(true, Ordering::Relaxed);
    }
}

#[derive(Debug)]
struct RetryResubmit {
    called: AtomicBool,
}

#[async_trait]
impl RetryStrategy<AdapterResubmit> for RetryResubmit {
    async fn handle_dropped(
        &self,
        pending: &bulkmail::adapter::PendingTransaction<AdapterResubmit>,
        _client: &ClientFlaky,
        _fees: &FeeResubmit,
        _replay: &ReplayResubmit,
    ) -> RetryDecision<AdapterResubmit> {
        if self.called.swap(true, Ordering::Relaxed) {
            RetryDecision::Abandon
        } else {
            RetryDecision::Resubmit {
                fee: pending.fee + 1,
                replay_token: pending.replay_token, // reuse same token
            }
        }
    }

    async fn handle_confirmed(
        &self,
        _pending: &bulkmail::adapter::PendingTransaction<AdapterResubmit>,
        _fees: &FeeResubmit,
        _replay: &ReplayResubmit,
        _confirmation_time: Duration,
    ) {
    }
}

impl ChainAdapter for AdapterResubmit {
    type FeeParams = u64;
    type ReplayToken = u64;
    type TxId = u64;
    type Client = ClientFlaky;
    type FeeManager = FeeResubmit;
    type ReplayProtection = ReplayResubmit;
    type RetryStrategy = RetryResubmit;
}

#[tokio::test]
async fn resubmit_reuses_replay_token() {
    let (_tx, rx) = mpsc::channel::<u64>(1);

    let replay = Arc::new(ReplayResubmit::new());

    let client = Arc::new(ClientFlaky {
        receiver: Arc::new(Mutex::new(Some(rx))),
        attempts: Arc::new(AtomicU64::new(0)),
    });

    let sender = Sender::<AdapterResubmit>::new(
        client.clone(),
        Arc::new(FeeResubmit),
        replay.clone(),
        Arc::new(RetryResubmit {
            called: AtomicBool::new(false),
        }),
    );

    sender.add_message(Message::default()).await;

    let run_sender = sender.clone();
    let run_handle = tokio::spawn(async move { run_sender.run().await });

    let attempts = client.attempts.clone();
    timeout(Duration::from_secs(3), async {
        while attempts.load(Ordering::Relaxed) < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("resubmit did not retry");
    run_handle.abort();
    assert!(!replay.released.load(Ordering::Relaxed));
}

// -----------------------------------------------------------------------------
// Adapter: Requeue releases token
// -----------------------------------------------------------------------------
#[derive(Debug)]
struct AdapterRequeue;

#[derive(Debug, Default)]
struct FeeRequeue;

#[async_trait]
impl FeeManager<AdapterRequeue> for FeeRequeue {
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

#[derive(Debug, Clone)]
struct ClientDropping {
    receiver: Arc<Mutex<Option<BlockReceiver>>>,
}

#[async_trait]
impl ChainClient<AdapterRequeue> for ClientDropping {
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
    ) -> Result<SendOutcome<AdapterRequeue>, Error> {
        Ok(SendOutcome::Dropped { tx_id: 1 })
    }

    async fn get_transaction_status(&self, _id: &u64) -> Result<TransactionStatus, Error> {
        Ok(TransactionStatus::Pending)
    }
}

#[derive(Debug)]
struct ReplayRequeue {
    released: AtomicBool,
    next_value: AtomicU64,
}

impl ReplayRequeue {
    fn new() -> Self {
        Self {
            released: AtomicBool::new(false),
            next_value: AtomicU64::new(7),
        }
    }
}

#[async_trait]
impl ReplayProtection<AdapterRequeue> for ReplayRequeue {
    async fn next(&self) -> u64 {
        self.next_value.fetch_add(1, Ordering::Relaxed)
    }

    async fn sync(&self) -> Result<(), Error> {
        Ok(())
    }

    async fn release(&self, _token: &u64) {
        self.released.store(true, Ordering::Relaxed);
    }
}

#[derive(Debug)]
struct RetryRequeue;

#[async_trait]
impl RetryStrategy<AdapterRequeue> for RetryRequeue {
    async fn handle_dropped(
        &self,
        _pending: &bulkmail::adapter::PendingTransaction<AdapterRequeue>,
        _client: &ClientDropping,
        _fees: &FeeRequeue,
        _replay: &ReplayRequeue,
    ) -> RetryDecision<AdapterRequeue> {
        RetryDecision::Requeue
    }

    async fn handle_confirmed(
        &self,
        _pending: &bulkmail::adapter::PendingTransaction<AdapterRequeue>,
        _fees: &FeeRequeue,
        _replay: &ReplayRequeue,
        _confirmation_time: Duration,
    ) {
    }
}

impl ChainAdapter for AdapterRequeue {
    type FeeParams = u64;
    type ReplayToken = u64;
    type TxId = u64;
    type Client = ClientDropping;
    type FeeManager = FeeRequeue;
    type ReplayProtection = ReplayRequeue;
    type RetryStrategy = RetryRequeue;
}

#[tokio::test]
async fn requeue_releases_replay_token() {
    let (_tx, rx) = mpsc::channel::<u64>(2);

    let replay = Arc::new(ReplayRequeue::new());

    let sender = Sender::<AdapterRequeue>::new(
        Arc::new(ClientDropping {
            receiver: Arc::new(Mutex::new(Some(rx))),
        }),
        Arc::new(FeeRequeue),
        replay.clone(),
        Arc::new(RetryRequeue),
    );

    sender.add_message(Message::default()).await;

    let run_sender = sender.clone();
    let run_handle = tokio::spawn(async move { run_sender.run().await });

    // Wait for release flag to be set
    timeout(Duration::from_secs(2), async {
        while !replay.released.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("replay token not released on requeue");

    run_handle.abort();
}

// -----------------------------------------------------------------------------
// Adapter: Abandon releases token
// -----------------------------------------------------------------------------
#[derive(Debug)]
struct AdapterAbandon;

#[derive(Debug, Default)]
struct FeeAbandon;

#[async_trait]
impl FeeManager<AdapterAbandon> for FeeAbandon {
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

#[derive(Debug, Clone)]
struct ClientAbandon {
    receiver: Arc<Mutex<Option<BlockReceiver>>>,
}

#[async_trait]
impl ChainClient<AdapterAbandon> for ClientAbandon {
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
    ) -> Result<SendOutcome<AdapterAbandon>, Error> {
        Ok(SendOutcome::Dropped { tx_id: 1 })
    }

    async fn get_transaction_status(&self, _id: &u64) -> Result<TransactionStatus, Error> {
        Ok(TransactionStatus::Pending)
    }
}

#[derive(Debug)]
struct ReplayAbandon {
    released: AtomicBool,
    next_value: AtomicU64,
}

impl ReplayAbandon {
    fn new() -> Self {
        Self {
            released: AtomicBool::new(false),
            next_value: AtomicU64::new(11),
        }
    }
}

#[async_trait]
impl ReplayProtection<AdapterAbandon> for ReplayAbandon {
    async fn next(&self) -> u64 {
        self.next_value.fetch_add(1, Ordering::Relaxed)
    }

    async fn sync(&self) -> Result<(), Error> {
        Ok(())
    }

    async fn release(&self, _token: &u64) {
        self.released.store(true, Ordering::Relaxed);
    }
}

#[derive(Debug)]
struct RetryAbandon;

#[async_trait]
impl RetryStrategy<AdapterAbandon> for RetryAbandon {
    async fn handle_dropped(
        &self,
        _pending: &bulkmail::adapter::PendingTransaction<AdapterAbandon>,
        _client: &ClientAbandon,
        _fees: &FeeAbandon,
        _replay: &ReplayAbandon,
    ) -> RetryDecision<AdapterAbandon> {
        RetryDecision::Abandon
    }

    async fn handle_confirmed(
        &self,
        _pending: &bulkmail::adapter::PendingTransaction<AdapterAbandon>,
        _fees: &FeeAbandon,
        _replay: &ReplayAbandon,
        _confirmation_time: Duration,
    ) {
    }
}

impl ChainAdapter for AdapterAbandon {
    type FeeParams = u64;
    type ReplayToken = u64;
    type TxId = u64;
    type Client = ClientAbandon;
    type FeeManager = FeeAbandon;
    type ReplayProtection = ReplayAbandon;
    type RetryStrategy = RetryAbandon;
}

#[tokio::test]
async fn abandon_releases_replay_token() {
    let (_tx, rx) = mpsc::channel::<u64>(1);

    let replay = Arc::new(ReplayAbandon::new());

    let sender = Sender::<AdapterAbandon>::new(
        Arc::new(ClientAbandon {
            receiver: Arc::new(Mutex::new(Some(rx))),
        }),
        Arc::new(FeeAbandon),
        replay.clone(),
        Arc::new(RetryAbandon),
    );

    sender.add_message(Message::default()).await;

    let run_sender = sender.clone();
    let run_handle = tokio::spawn(async move { run_sender.run().await });

    // Wait for release flag to be set
    timeout(Duration::from_secs(2), async {
        while !replay.released.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("replay token not released on abandon");

    run_handle.abort();
}
