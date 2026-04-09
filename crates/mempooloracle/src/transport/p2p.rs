#[cfg(feature = "reth-p2p")]
mod enabled {
    use alloy::consensus::Transaction as _;
    use crate::{
        Address, BlockUpdate, ConsensusTransportImplementation, MempoolEvent, MempoolTracker,
        P2pBlockTransport, P2pTransportConfig, PendingTx, TrackerConfig, TrackerError,
        TrackerReset, TrackerRuntime, TxId,
        runtime::{DEFAULT_SHUTDOWN_VALUE, RuntimeTelemetry, TransportKind},
    };
    use futures::{StreamExt, channel::mpsc as futures_mpsc};
    use reth_ethereum::{
        TransactionSigned,
        network::{NetworkConfig, NetworkEvent, NetworkEventListenerProvider, NetworkManager, events::PeerEvent},
        pool::{
            CoinbaseTipOrdering, EthPooledTransaction, Pool, TransactionListenerKind,
            TransactionPool, blobstore::InMemoryBlobStore, test_utils::OkValidator,
        },
        provider::test_utils::NoopProvider,
        primitives::SignerRecoverable as _,
    };
    use reth_network_peers::TrustedPeer;
    use std::{
        collections::{BTreeMap, HashSet},
        path::PathBuf,
        str::FromStr,
        sync::{Arc, mpsc},
        thread,
        time::{SystemTime, UNIX_EPOCH},
    };
    use tokio::{
        sync::watch,
        task::JoinHandle,
    };

    type EmbeddedPool = Pool<
        OkValidator<EthPooledTransaction>,
        CoinbaseTipOrdering<EthPooledTransaction>,
        InMemoryBlobStore,
    >;

    pub async fn connect_with_config(
        config: P2pTransportConfig,
        tracker_config: TrackerConfig,
    ) -> Result<TrackerRuntime, TrackerError> {
        if config.chain != "mainnet" {
            return Err(TrackerError::UnsupportedTransport(
                "embedded p2p currently supports mainnet only",
            ));
        }

        let consensus_config = match &config.block_transport {
            P2pBlockTransport::Consensus(consensus) => consensus,
            P2pBlockTransport::ExecutionPolling => {
                return Err(TrackerError::UnsupportedTransport(
                    "execution-only block polling is no longer supported",
                ));
            }
        };

        #[cfg(not(feature = "consensus-p2p"))]
        {
            let _ = consensus_config;
            return Err(TrackerError::FeatureDisabled("consensus-p2p"));
        }

        #[cfg(feature = "consensus-p2p")]
        {
            if !matches!(
                consensus_config.implementation,
                ConsensusTransportImplementation::Eth2Libp2p
            ) {
                return Err(TrackerError::UnsupportedTransport(
                    "unsupported consensus p2p implementation",
                ));
            }
        }

        let client = NoopProvider::default();
        let pool: EmbeddedPool = Pool::new(
            OkValidator::default(),
            CoinbaseTipOrdering::default(),
            InMemoryBlobStore::default(),
            Default::default(),
        );

        let local_key = reth_ethereum::network::config::rng_secret_key();
        let mut builder = NetworkConfig::builder(local_key);

        if let Some(addr) = config.listen_addr {
            builder = builder.set_addrs(addr);
        }

        if config.bootnodes.is_empty() {
            builder = builder.mainnet_boot_nodes();
        } else {
            let boot_nodes = parse_execution_bootnodes(&config.bootnodes)?;
            builder = builder.boot_nodes(boot_nodes);
        }

        if !config.discovery_v4 {
            builder = builder.disable_discv4_discovery();
        }

        let network_config = builder.build(client);
        let transactions_manager_config = network_config.transactions_manager_config.clone();

        let (network_handle, network, txpool, _eth) = NetworkManager::builder(network_config)
            .await
            .map_err(|err| TrackerError::Setup(err.to_string()))?
            .transactions(pool.clone(), transactions_manager_config)
            .split_with_handle();

        let (event_tx, event_rx) = mpsc::channel();
        let telemetry = Arc::new(RuntimeTelemetry::new(TransportKind::P2p));
        let (shutdown_tx, shutdown_rx) = watch::channel(DEFAULT_SHUTDOWN_VALUE);

        let (handle, tracker) = MempoolTracker::from_channel(event_rx, &[], tracker_config);
        thread::spawn(move || tracker.run());

        let network_task = tokio::spawn(network);
        let txpool_task = tokio::spawn(txpool);
        let pending_task = tokio::spawn(run_pending_listener(
            pool.clone(),
            event_tx.clone(),
            telemetry.clone(),
            shutdown_rx.clone(),
        ));
        let backfill_task = tokio::spawn(run_backfill(
            pool.clone(),
            event_tx.clone(),
            telemetry.clone(),
            shutdown_rx.clone(),
        ));
        let peer_events_task = tokio::spawn(run_execution_peer_event_listener(
            network_handle,
            telemetry.clone(),
            shutdown_rx.clone(),
        ));

        #[cfg(feature = "consensus-p2p")]
        let consensus_task = tokio::spawn(run_consensus_block_listener(
            config,
            pool,
            event_tx,
            telemetry.clone(),
            shutdown_rx,
        ));

        let mut tasks: Vec<JoinHandle<()>> =
            vec![network_task, txpool_task, pending_task, backfill_task, peer_events_task];

        #[cfg(feature = "consensus-p2p")]
        tasks.push(consensus_task);

        Ok(TrackerRuntime::new(handle, telemetry, shutdown_tx, tasks))
    }

    fn parse_execution_bootnodes(bootnodes: &[String]) -> Result<Vec<TrustedPeer>, TrackerError> {
        bootnodes
            .iter()
            .map(|node| {
                TrustedPeer::from_str(node).map_err(|err| {
                    TrackerError::Setup(format!(
                        "failed to parse execution bootnode `{node}`: {err}"
                    ))
                })
            })
            .collect()
    }

    async fn run_backfill(
        pool: EmbeddedPool,
        event_tx: mpsc::Sender<MempoolEvent>,
        telemetry: Arc<RuntimeTelemetry>,
        mut shutdown: watch::Receiver<bool>,
    ) {
        let existing = pool
            .pooled_transactions()
            .into_iter()
            .map(|tx| pending_tx_from_reth(&tx))
            .collect::<Vec<_>>();

        telemetry.record_backfill_size(existing.len());

        for tx in existing {
            tokio::select! {
                _ = shutdown.changed() => break,
                else => {
                    if event_tx.send(MempoolEvent::PendingTransaction(tx)).is_err() {
                        break;
                    }
                }
            }
        }
    }

    async fn replay_pool_snapshot(pool: &EmbeddedPool, event_tx: &mpsc::Sender<MempoolEvent>) -> bool {
        for tx in pool
            .pooled_transactions()
            .into_iter()
            .map(|tx| pending_tx_from_reth(&tx))
        {
            if event_tx.send(MempoolEvent::PendingTransaction(tx)).is_err() {
                return false;
            }
        }

        true
    }

    async fn run_pending_listener(
        pool: EmbeddedPool,
        event_tx: mpsc::Sender<MempoolEvent>,
        telemetry: Arc<RuntimeTelemetry>,
        mut shutdown: watch::Receiver<bool>,
    ) {
        let mut listener = pool.pending_transactions_listener_for(TransactionListenerKind::All);

        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                maybe_hash = listener.recv() => {
                    let Some(hash) = maybe_hash else {
                        break;
                    };

                    let Some(tx) = pool.get(&hash) else {
                        continue;
                    };

                    telemetry.record_pending_seen();
                    telemetry.record_p2p_import();

                    if event_tx
                        .send(MempoolEvent::PendingTransaction(pending_tx_from_reth(&tx)))
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    }

    async fn run_execution_peer_event_listener(
        network_handle: reth_ethereum::network::NetworkHandle<reth_ethereum::network::EthNetworkPrimitives>,
        telemetry: Arc<RuntimeTelemetry>,
        mut shutdown: watch::Receiver<bool>,
    ) {
        let mut events = network_handle.event_listener();
        let mut peers = HashSet::new();

        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                maybe_event = events.next() => {
                    let Some(event) = maybe_event else {
                        break;
                    };

                    match event {
                        NetworkEvent::Peer(PeerEvent::SessionEstablished(info)) => {
                            peers.insert(info.peer_id);
                            telemetry.record_p2p_peer_count(peers.len());
                        }
                        NetworkEvent::Peer(PeerEvent::SessionClosed { peer_id, .. }) |
                        NetworkEvent::Peer(PeerEvent::PeerRemoved(peer_id)) => {
                            peers.remove(&peer_id);
                            telemetry.record_p2p_peer_count(peers.len());
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    fn pending_tx_from_signed(tx: &TransactionSigned) -> Option<PendingTx> {
        let sender = tx.recover_signer().ok()?;
        Some(PendingTx {
            id: TxId(tx.tx_hash().0),
            sender: Address(sender.0 .0),
            nonce: tx.nonce(),
            max_fee_per_gas: tx.max_fee_per_gas(),
            max_priority_fee_per_gas: tx.max_priority_fee_per_gas().unwrap_or_default(),
            gas_limit: tx.gas_limit(),
        })
    }

    fn pending_tx_from_reth(
        tx: &reth_ethereum::pool::ValidPoolTransaction<EthPooledTransaction>,
    ) -> PendingTx {
        PendingTx {
            id: TxId(tx.hash().0),
            sender: Address(tx.sender().0.0),
            nonce: tx.nonce(),
            max_fee_per_gas: tx.max_fee_per_gas(),
            max_priority_fee_per_gas: tx
                .transaction
                .max_priority_fee_per_gas()
                .unwrap_or_else(|| tx.priority_fee_or_price()),
            gas_limit: tx.gas_limit(),
        }
    }

    #[cfg(feature = "consensus-p2p")]
    mod consensus {
        use super::*;
        use alloy::eips::Decodable2718;
        use eth2_libp2p::{
            Context, Enr, MessageAcceptance, NetworkConfig as ConsensusNetworkConfig, NetworkEvent,
            PeerId, PubsubMessage, Response,
            libp2p::identity::secp256k1,
            rpc::{RequestType, StatusMessage, StatusMessageV2},
            rpc::methods::OldBlocksByRangeRequest,
            service::Network as ConsensusNetwork,
            service::api_types::AppRequestId,
            types::{EnrForkId, ForkContext, GossipKind},
        };
        use grandine_types::{
            combined::{ExecutionPayload as CombinedExecutionPayload, SignedBeaconBlock as CombinedSignedBeaconBlock},
            config::Config as ChainConfig,
            nonstandard::Phase,
            preset::Mainnet,
            traits::SignedBeaconBlock as _,
        };
        use std::{collections::HashMap, fs, net::Ipv4Addr, sync::Arc};

        const MAX_RECOVERY_RANGE: u64 = 128;

        #[derive(Clone)]
        struct ObservedBlock {
            number: u64,
            slot: u64,
            update: BlockUpdate,
            source: PeerId,
        }

        struct RecoveryRequest {
            request_id: usize,
            target_number: u64,
            target_block: ObservedBlock,
        }

        struct ConsensusState {
            last_emitted_number: Option<u64>,
            last_emitted_slot: Option<u64>,
            buffered: BTreeMap<u64, ObservedBlock>,
            connected_peers: HashSet<PeerId>,
            peer_statuses: HashMap<PeerId, StatusMessage>,
            recovery: Option<RecoveryRequest>,
            next_request_id: usize,
        }

        impl ConsensusState {
            fn new() -> Self {
                Self {
                    last_emitted_number: None,
                    last_emitted_slot: None,
                    buffered: BTreeMap::new(),
                    connected_peers: HashSet::new(),
                    peer_statuses: HashMap::new(),
                    recovery: None,
                    next_request_id: 1,
                }
            }

            fn next_app_request_id(&mut self) -> AppRequestId {
                let id = self.next_request_id;
                self.next_request_id += 1;
                AppRequestId::Application(id)
            }
        }

        pub(super) async fn run_consensus_block_listener(
            config: P2pTransportConfig,
            pool: EmbeddedPool,
            event_tx: mpsc::Sender<MempoolEvent>,
            telemetry: Arc<RuntimeTelemetry>,
            mut shutdown: watch::Receiver<bool>,
        ) {
            let chain_config = Arc::new(ChainConfig::mainnet());
            let fork_phase = latest_enabled_phase(&chain_config);
            let network_dir = consensus_network_dir();

            if fs::create_dir_all(&network_dir).is_err() {
                return;
            }

            let mut consensus_config = ConsensusNetworkConfig::default();
            consensus_config.network_dir = Some(network_dir);

            if let Some(addr) = config.listen_addr {
                let port = addr.port();
                consensus_config.set_ipv4_listening_address(Ipv4Addr::UNSPECIFIED, port, port, port);
                consensus_config.enr_address = (Some(Ipv4Addr::LOCALHOST), None);
            }

            for node in &config.bootnodes {
                if let Ok(enr) = Enr::from_str(node) {
                    consensus_config.boot_nodes_enr.push(enr);
                } else if let Ok(addr) = eth2_libp2p::Multiaddr::from_str(node) {
                    consensus_config.boot_nodes_multiaddr.push(addr);
                }
            }

            if !config.discovery_v4 {
                consensus_config.disable_discovery = true;
            }

            let (shutdown_tx, _) = futures_mpsc::channel(1);
            let executor = eth2_libp2p::TaskExecutor::new(shutdown_tx);
            let fork_context = Arc::new(ForkContext::dummy::<Mainnet>(&chain_config, fork_phase));
            let custody_group_count = chain_config.custody_requirement;
            let context = Context {
                chain_config: chain_config.clone(),
                config: Arc::new(consensus_config),
                enr_fork_id: EnrForkId::default(),
                fork_context: fork_context.clone(),
                libp2p_registry: None,
            };

            let Ok((mut service, _globals)) = ConsensusNetwork::new(
                chain_config,
                executor,
                context,
                custody_group_count,
                secp256k1::Keypair::generate().into(),
            )
            .await else {
                return;
            };

            service.subscribe_kind(GossipKind::BeaconBlock);

            let mut state = ConsensusState::new();

            loop {
                tokio::select! {
                    _ = shutdown.changed() => break,
                    event = service.next_event() => {
                        if !handle_network_event(
                            &mut service,
                            &mut state,
                            &pool,
                            &event_tx,
                            &telemetry,
                            &fork_context,
                            event,
                        ).await {
                            break;
                        }
                    }
                }
            }
        }

        async fn handle_network_event(
            service: &mut ConsensusNetwork<Mainnet>,
            state: &mut ConsensusState,
            pool: &EmbeddedPool,
            event_tx: &mpsc::Sender<MempoolEvent>,
            telemetry: &Arc<RuntimeTelemetry>,
            fork_context: &Arc<ForkContext>,
            event: NetworkEvent<Mainnet>,
        ) -> bool {
            match event {
                NetworkEvent::PeerConnectedIncoming(peer_id) | NetworkEvent::PeerConnectedOutgoing(peer_id) => {
                    state.connected_peers.insert(peer_id);
                    telemetry.record_consensus_peer_count(state.connected_peers.len());
                }
                NetworkEvent::PeerDisconnected(peer_id) => {
                    state.connected_peers.remove(&peer_id);
                    state.peer_statuses.remove(&peer_id);
                    telemetry.record_consensus_peer_count(state.connected_peers.len());
                }
                NetworkEvent::StatusPeer(peer_id) => {
                    let _ = service.send_request(
                        peer_id,
                        state.next_app_request_id(),
                        RequestType::Status(local_status(state, fork_context)),
                    );
                }
                NetworkEvent::RequestReceived {
                    peer_id,
                    inbound_request_id,
                    request_type: RequestType::Status(remote),
                } => {
                    state.peer_statuses.insert(peer_id, remote);
                    service.send_response(
                        peer_id,
                        inbound_request_id,
                        Response::Status(local_status(state, fork_context)),
                    );
                }
                NetworkEvent::ResponseReceived {
                    peer_id,
                    response: Response::Status(status),
                    ..
                } => {
                    state.peer_statuses.insert(peer_id, status);
                }
                NetworkEvent::PubsubMessage {
                    id,
                    source,
                    message: PubsubMessage::BeaconBlock(block),
                    ..
                } => {
                    service.report_message_validation_result(&source, id, MessageAcceptance::Accept);
                    if let Some(observed) = observed_block_from_beacon_block(block, source) {
                        telemetry.record_consensus_last_block(observed.number);
                        if !ingest_block(service, state, pool, event_tx, telemetry, observed).await {
                            return false;
                        }
                    }
                }
                NetworkEvent::ResponseReceived {
                    response: Response::BlocksByRange(block),
                    app_request_id: AppRequestId::Application(request_id),
                    ..
                } => {
                    if let Some(recovery) = &state.recovery {
                        if recovery.request_id != request_id {
                            return true;
                        }
                    } else {
                        return true;
                    }

                    match block {
                        Some(block) => {
                            if let Some(observed) = observed_block_from_beacon_block(block, PeerId::random()) {
                                if let Some(last) = state.last_emitted_number {
                                    if observed.number > last {
                                        state.buffered.entry(observed.number).or_insert(observed);
                                    }
                                }
                            }
                        }
                        None => {
                            if !complete_recovery(state, pool, event_tx, telemetry).await {
                                return false;
                            }
                        }
                    }
                }
                NetworkEvent::RPCFailed {
                    app_request_id: AppRequestId::Application(request_id),
                    ..
                } => {
                    if state
                        .recovery
                        .as_ref()
                        .is_some_and(|recovery| recovery.request_id == request_id)
                    {
                        if !reset_from_recovery_target(state, pool, event_tx, telemetry).await {
                            return false;
                        }
                    }
                }
                _ => {}
            }

            true
        }

        async fn ingest_block(
            service: &mut ConsensusNetwork<Mainnet>,
            state: &mut ConsensusState,
            pool: &EmbeddedPool,
            event_tx: &mpsc::Sender<MempoolEvent>,
            telemetry: &Arc<RuntimeTelemetry>,
            observed: ObservedBlock,
        ) -> bool {
            match state.last_emitted_number {
                None => {
                    return emit_anchor(state, pool, event_tx, telemetry, observed).await;
                }
                Some(last) if observed.number <= last => return true,
                Some(last) if observed.number == last + 1 => {
                    if !emit_block(state, event_tx, telemetry, observed).await {
                        return false;
                    }
                    return drain_buffered_blocks(state, event_tx, telemetry).await;
                }
                Some(_) => {}
            }

            state.buffered.entry(observed.number).or_insert(observed.clone());

            if state.recovery.is_none() {
                let last_slot = state.last_emitted_slot.unwrap_or(observed.slot);
                let slot_gap = observed.slot.saturating_sub(last_slot);

                if slot_gap == 0 || slot_gap > MAX_RECOVERY_RANGE {
                    return reset_with_anchor(state, pool, event_tx, telemetry, observed).await;
                }

                let request_id = match state.next_app_request_id() {
                    AppRequestId::Application(id) => id,
                    AppRequestId::Internal => unreachable!("application ids are generated locally"),
                };

                let request = RequestType::BlocksByRange(OldBlocksByRangeRequest::new(
                    last_slot.saturating_add(1),
                    slot_gap,
                    1,
                ));

                if service
                    .send_request(
                        observed.source,
                        AppRequestId::Application(request_id),
                        request,
                    )
                    .is_err()
                {
                    return reset_with_anchor(state, pool, event_tx, telemetry, observed).await;
                }

                state.recovery = Some(RecoveryRequest {
                    request_id,
                    target_number: observed.number,
                    target_block: observed,
                });
            }

            true
        }

        async fn complete_recovery(
            state: &mut ConsensusState,
            pool: &EmbeddedPool,
            event_tx: &mpsc::Sender<MempoolEvent>,
            telemetry: &Arc<RuntimeTelemetry>,
        ) -> bool {
            let Some(recovery) = state.recovery.take() else {
                return true;
            };

            let before = state.last_emitted_number.unwrap_or_default();
            let drained = drain_buffered_blocks(state, event_tx, telemetry).await;
            if !drained {
                return false;
            }

            if state.last_emitted_number.unwrap_or_default() >= recovery.target_number {
                let recovered = state
                    .last_emitted_number
                    .unwrap_or_default()
                    .saturating_sub(before);
                if recovered > 0 {
                    telemetry.record_consensus_recovered_blocks(recovered);
                }
                return true;
            }

            reset_with_anchor(state, pool, event_tx, telemetry, recovery.target_block).await
        }

        async fn emit_anchor(
            state: &mut ConsensusState,
            pool: &EmbeddedPool,
            event_tx: &mpsc::Sender<MempoolEvent>,
            telemetry: &Arc<RuntimeTelemetry>,
            anchor: ObservedBlock,
        ) -> bool {
            if event_tx
                .send(MempoolEvent::Reset(TrackerReset {
                    base_fee: anchor.update.new_base_fee,
                    gas_limit: anchor.update.gas_limit,
                }))
                .is_err()
            {
                return false;
            }

            if !replay_pool_snapshot(pool, event_tx).await {
                return false;
            }

            state.buffered.clear();
            emit_block(state, event_tx, telemetry, anchor).await
        }

        async fn reset_with_anchor(
            state: &mut ConsensusState,
            pool: &EmbeddedPool,
            event_tx: &mpsc::Sender<MempoolEvent>,
            telemetry: &Arc<RuntimeTelemetry>,
            anchor: ObservedBlock,
        ) -> bool {
            telemetry.record_consensus_gap_reset();
            state.recovery = None;
            emit_anchor(state, pool, event_tx, telemetry, anchor).await
        }

        async fn reset_from_recovery_target(
            state: &mut ConsensusState,
            pool: &EmbeddedPool,
            event_tx: &mpsc::Sender<MempoolEvent>,
            telemetry: &Arc<RuntimeTelemetry>,
        ) -> bool {
            let Some(recovery) = state.recovery.take() else {
                return true;
            };

            reset_with_anchor(state, pool, event_tx, telemetry, recovery.target_block).await
        }

        async fn emit_block(
            state: &mut ConsensusState,
            event_tx: &mpsc::Sender<MempoolEvent>,
            telemetry: &Arc<RuntimeTelemetry>,
            block: ObservedBlock,
        ) -> bool {
            let tx_count = block.update.included_txs.len();
            let gas_used = block.update.gas_used;
            let block_number = block.number;
            let next_expected = block.number.saturating_add(1);

            if event_tx.send(MempoolEvent::NewBlock(block.update)).is_err() {
                return false;
            }

            telemetry.record_block(tx_count, gas_used);
            telemetry.record_consensus_last_block(block_number);
            telemetry.record_consensus_next_expected(next_expected);
            if state.last_emitted_number.is_none() {
                telemetry.record_consensus_anchor(block_number);
            }

            state.last_emitted_number = Some(block_number);
            state.last_emitted_slot = Some(block.slot);
            true
        }

        async fn drain_buffered_blocks(
            state: &mut ConsensusState,
            event_tx: &mpsc::Sender<MempoolEvent>,
            telemetry: &Arc<RuntimeTelemetry>,
        ) -> bool {
            loop {
                let Some(next_expected) = state.last_emitted_number.map(|number| number + 1) else {
                    return true;
                };
                let Some(block) = state.buffered.remove(&next_expected) else {
                    return true;
                };

                if !emit_block(state, event_tx, telemetry, block).await {
                    return false;
                }
            }
        }

        fn latest_enabled_phase(chain_config: &ChainConfig) -> Phase {
            for phase in [
                Phase::Gloas,
                Phase::Fulu,
                Phase::Electra,
                Phase::Deneb,
                Phase::Capella,
                Phase::Bellatrix,
                Phase::Altair,
                Phase::Phase0,
            ] {
                if chain_config.is_phase_enabled::<Mainnet>(phase) {
                    return phase;
                }
            }

            Phase::Phase0
        }

        fn local_status(state: &ConsensusState, fork_context: &ForkContext) -> StatusMessage {
            let head_slot = state.last_emitted_slot.unwrap_or_default();
            let earliest_available_slot = state.last_emitted_slot.unwrap_or_default();

            StatusMessage::V2(StatusMessageV2 {
                fork_digest: fork_context.current_fork_digest(),
                finalized_root: grandine_types::phase0::primitives::H256::zero(),
                finalized_epoch: 0,
                head_root: grandine_types::phase0::primitives::H256::zero(),
                head_slot,
                earliest_available_slot,
            })
        }

        fn observed_block_from_beacon_block(
            block: Arc<CombinedSignedBeaconBlock<Mainnet>>,
            source: PeerId,
        ) -> Option<ObservedBlock> {
            let slot = block.message().slot();
            let payload = block.as_ref().clone().execution_payload()?;
            let (number, gas_used, gas_limit, new_base_fee, included_txs) = match payload {
                CombinedExecutionPayload::Bellatrix(payload) => (
                    payload.block_number,
                    payload.gas_used,
                    payload.gas_limit,
                    payload.base_fee_per_gas.into_raw().try_into().unwrap_or(u128::MAX),
                    decode_payload_transactions(payload.transactions.iter()),
                ),
                CombinedExecutionPayload::Capella(payload) => (
                    payload.block_number,
                    payload.gas_used,
                    payload.gas_limit,
                    payload.base_fee_per_gas.into_raw().try_into().unwrap_or(u128::MAX),
                    decode_payload_transactions(payload.transactions.iter()),
                ),
                CombinedExecutionPayload::Deneb(payload) => (
                    payload.block_number,
                    payload.gas_used,
                    payload.gas_limit,
                    payload.base_fee_per_gas.into_raw().try_into().unwrap_or(u128::MAX),
                    decode_payload_transactions(payload.transactions.iter()),
                ),
            };

            Some(ObservedBlock {
                number,
                slot,
                update: BlockUpdate {
                    included_txs,
                    new_base_fee,
                    gas_used,
                    gas_limit,
                },
                source,
            })
        }

        fn decode_payload_transactions<'a>(
            transactions: impl Iterator<
                Item = &'a grandine_types::bellatrix::primitives::Transaction<Mainnet>,
            >,
        ) -> Vec<PendingTx> {
            transactions
                .filter_map(
                    |raw: &'a grandine_types::bellatrix::primitives::Transaction<Mainnet>| {
                        TransactionSigned::decode_2718_exact(raw.as_bytes()).ok()
                    },
                )
                .filter_map(|tx| pending_tx_from_signed(&tx))
                .collect()
        }

        fn consensus_network_dir() -> PathBuf {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or_default();
            std::env::temp_dir().join(format!("mempooloracle-consensus-{unique}"))
        }
    }

    #[cfg(feature = "consensus-p2p")]
    use consensus::run_consensus_block_listener;
}

#[cfg(feature = "reth-p2p")]
pub use enabled::connect_with_config;

#[cfg(not(feature = "reth-p2p"))]
pub async fn connect_with_config(
    _config: crate::P2pTransportConfig,
    _tracker_config: crate::TrackerConfig,
) -> Result<crate::TrackerRuntime, crate::TrackerError> {
    Err(crate::TrackerError::FeatureDisabled("reth-p2p"))
}
