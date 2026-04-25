//! P2P network transport for mempool tracking.
#[cfg(feature = "reth-p2p")]
mod enabled {
    use crate::{
        Address, BlockNumber, BlockUpdate, ExecutionHash, MempoolEvent, MempoolTracker,
        P2pBlockTransport, P2pTransportConfig, PendingTx, Slot, TrackerConfig, TrackerError,
        TrackerPrune, TrackerRuntime,
        runtime::{DEFAULT_SHUTDOWN_VALUE, RuntimeTelemetry, TransportKind},
    };
    use alloy::consensus::Transaction as _;
    use alloy::primitives::{B256, U256};
    use futures::{StreamExt, channel::mpsc as futures_mpsc};
    use reth_ethereum::{
        TransactionSigned,
        chainspec::{Head, MAINNET as chainspecs},
        network::{
            NetworkConfig, NetworkEvent, NetworkEventListenerProvider, NetworkManager,
            events::{PeerEvent, SessionInfo},
        },
        pool::{
            CoinbaseTipOrdering, EthPooledTransaction, Pool, TransactionListenerKind,
            TransactionPool, blobstore::InMemoryBlobStore, test_utils::OkValidator,
        },
        primitives::SignerRecoverable as _,
        provider::test_utils::NoopProvider,
    };
    use reth_network_peers::TrustedPeer;
    use std::{
        collections::{HashMap, HashSet},
        path::PathBuf,
        str::FromStr,
        sync::{Arc, mpsc},
        thread,
        time::{SystemTime, UNIX_EPOCH},
    };
    use tokio::{sync::watch, task::JoinHandle};

    struct P2pAuditLogger {
        file: Option<std::io::BufWriter<std::fs::File>>,
    }

    impl P2pAuditLogger {
        fn new(path: Option<std::path::PathBuf>) -> Self {
            let file = path.and_then(|p| {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(p)
                    .ok()
                    .map(std::io::BufWriter::new)
            });
            Self { file }
        }

        fn log(&mut self, layer: &str, direction: &str, peer_id: &str, event: &str) {
            if let Some(ref mut writer) = self.file {
                use std::io::Write;
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis();
                let _ = writeln!(writer, "[{now}] {layer:4} {direction:3} {peer_id} {event}");
                let _ = writer.flush();
            }
        }
    }

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

        match &config.block_transport {
            P2pBlockTransport::Consensus(_) => {}
            P2pBlockTransport::ExecutionPolling => {
                return Err(TrackerError::UnsupportedTransport(
                    "execution-only block polling is no longer supported",
                ));
            }
        };

        #[cfg(not(feature = "consensus-p2p"))]
        {
            return Err(TrackerError::FeatureDisabled("consensus-p2p"));
        }

        #[cfg(feature = "consensus-p2p")]
        {
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
            } else if let Some(port) = config.execution_port {
                builder = builder.set_addrs(std::net::SocketAddr::from(([0, 0, 0, 0], port)));
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

            let mut network_config = builder.build(client);
            network_config.chain_id = chainspecs.chain().id();

            let transactions_manager_config = network_config.transactions_manager_config.clone();

            let (network_handle, network, txpool, _eth) = NetworkManager::builder(network_config)
                .await
                .map_err(|err| TrackerError::Setup(err.to_string()))?
                .transactions(pool.clone(), transactions_manager_config)
                .split_with_handle();

            // Initial status update
            let head_hash = B256::from_str(
                "0x41110c60043ad922dc366d7a54a31e8b802233d095e59e139621200b1aea67f8",
            )
            .unwrap();
            network_handle.update_status(Head {
                number: 24898298,
                hash: head_hash,
                difficulty: U256::ZERO,
                total_difficulty: U256::ZERO,
                timestamp: 1776414463,
            });

            let (event_tx, event_rx) = mpsc::channel();
            let telemetry = Arc::new(RuntimeTelemetry::new(TransportKind::P2p));
            let (shutdown_tx, shutdown_rx) = watch::channel(DEFAULT_SHUTDOWN_VALUE);

            let (handle, tracker) = MempoolTracker::from_channel(event_rx, &[], tracker_config);
            let _tracker_task = thread::spawn(move || tracker.run());

            let network_task = tokio::spawn(network);
            let txpool_task = tokio::spawn(txpool);
            let pending_task = tokio::spawn(run_pending_listener(
                pool.clone(),
                event_tx.clone(),
                telemetry.clone(),
                config.log_path.clone(),
                shutdown_rx.clone(),
            ));
            let backfill_task = tokio::spawn(run_backfill(
                pool.clone(),
                event_tx.clone(),
                telemetry.clone(),
                config.log_path.clone(),
                shutdown_rx.clone(),
            ));
            let peer_events_task = tokio::spawn(run_execution_peer_event_listener(
                network_handle.clone(),
                telemetry.clone(),
                config.log_path.clone(),
                shutdown_rx.clone(),
            ));

            #[cfg(feature = "consensus-p2p")]
            let consensus_task = tokio::spawn(consensus::run_consensus_block_listener(
                config,
                pool,
                event_tx,
                telemetry.clone(),
                network_handle,
                shutdown_rx,
            ));

            let mut tasks: Vec<JoinHandle<()>> = vec![
                network_task,
                txpool_task,
                pending_task,
                backfill_task,
                peer_events_task,
            ];

            #[cfg(feature = "consensus-p2p")]
            tasks.push(consensus_task);

            Ok(TrackerRuntime::new(handle, telemetry, shutdown_tx, tasks))
        }
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

    async fn run_execution_peer_event_listener(
        network_handle: reth_ethereum::network::NetworkHandle<
            reth_ethereum::network::EthNetworkPrimitives,
        >,
        telemetry: Arc<RuntimeTelemetry>,
        log_path: Option<PathBuf>,
        mut shutdown: watch::Receiver<bool>,
    ) {
        let mut events = network_handle.event_listener();
        let mut peers = HashSet::new();
        let mut peer_directions = HashMap::new();
        let mut audit_log = P2pAuditLogger::new(log_path);

        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                maybe_event = events.next() => {
                    let Some(event) = maybe_event else {
                        break;
                    };

                    match event {
                        NetworkEvent::ActivePeerSession { info, .. } => {
                            tracing::debug!(peer_id = %info.peer_id, "Execution peer session active");
                            let is_ingress = false;
                            let dir_str = if is_ingress { "IN " } else { "OUT" };
                            let peer_id = info.peer_id.to_string();
                            let detail = execution_session_detail(&info);
                            audit_log.log("EL", dir_str, &peer_id, &format!("SessionActive {detail}"));
                            telemetry.record_peer_event("EL", dir_str.trim(), peer_id.clone(), "SessionActive", detail);

                            if peers.insert(info.peer_id) {
                                peer_directions.insert(info.peer_id, is_ingress);
                                telemetry.record_el_connection(peer_id, Some(is_ingress));
                            }
                            telemetry.record_p2p_peer_count(peers.len());
                        }
                        NetworkEvent::Peer(PeerEvent::SessionEstablished(info)) => {
                            tracing::debug!(peer_id = %info.peer_id, "Execution peer connected");
                            let is_ingress = false;
                            let dir_str = if is_ingress { "IN " } else { "OUT" };
                            let peer_id = info.peer_id.to_string();
                            let detail = execution_session_detail(&info);
                            audit_log.log("EL", dir_str, &peer_id, &format!("Connected {detail}"));
                            telemetry.record_peer_event("EL", dir_str.trim(), peer_id.clone(), "Connected", detail);

                            if peers.insert(info.peer_id) {
                                peer_directions.insert(info.peer_id, is_ingress);
                                telemetry.record_el_connection(peer_id, Some(is_ingress));
                            }
                            telemetry.record_p2p_peer_count(peers.len());
                        }
                        NetworkEvent::Peer(PeerEvent::SessionClosed { peer_id, reason }) => {
                            tracing::debug!(peer_id = %peer_id, "Execution peer session closed");
                            let detail = reason
                                .map(|reason| format!("reason={reason:?}"))
                                .unwrap_or_else(|| "reason=unknown".to_owned());
                            audit_log.log("EL", "END", &peer_id.to_string(), &format!("SessionClosed {detail}"));
                            telemetry.record_peer_event("EL", "END", peer_id.to_string(), "SessionClosed", detail);
                            peers.remove(&peer_id);
                            if let Some(is_ingress) = peer_directions.remove(&peer_id) {
                                telemetry.record_el_disconnection(Some(is_ingress));
                            }
                            telemetry.record_p2p_peer_count(peers.len());
                        }
                        NetworkEvent::Peer(PeerEvent::PeerAdded(peer_id)) => {
                            tracing::debug!(peer_id = %peer_id, "Execution peer added");
                            audit_log.log("EL", "INF", &peer_id.to_string(), "PeerAdded");
                            telemetry.record_peer_event("EL", "INF", peer_id.to_string(), "PeerAdded", String::new());
                        }
                        NetworkEvent::Peer(PeerEvent::PeerRemoved(peer_id)) => {
                            tracing::debug!(peer_id = %peer_id, "Execution peer removed");
                            audit_log.log("EL", "END", &peer_id.to_string(), "PeerRemoved");
                            telemetry.record_peer_event("EL", "END", peer_id.to_string(), "PeerRemoved", String::new());
                            peers.remove(&peer_id);
                            if let Some(is_ingress) = peer_directions.remove(&peer_id) {
                                telemetry.record_el_disconnection(Some(is_ingress));
                            }
                            telemetry.record_p2p_peer_count(peers.len());
                        }
                    }
                }
            }
        }
    }

    fn execution_session_detail(info: &SessionInfo) -> String {
        format!(
            "addr={} client={} version={:?} kind={:?} caps={:?}",
            info.remote_addr,
            info.client_version,
            info.version,
            info.peer_kind,
            info.capabilities.capabilities()
        )
    }

    async fn run_pending_listener(
        pool: EmbeddedPool,
        event_tx: mpsc::Sender<MempoolEvent>,
        telemetry: Arc<RuntimeTelemetry>,
        log_path: Option<PathBuf>,
        mut shutdown: watch::Receiver<bool>,
    ) {
        let mut listener = pool.pending_transactions_listener_for(TransactionListenerKind::All);
        let mut audit_log = P2pAuditLogger::new(log_path);

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

                    audit_log.log("EL", "IN ", "pool", &format!("NewPendingTx {hash:?}"));
                    telemetry.record_pending_seen();
                    telemetry.record_p2p_import();
                    telemetry.record_el_txs_received();

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

    async fn run_backfill(
        pool: EmbeddedPool,
        event_tx: mpsc::Sender<MempoolEvent>,
        telemetry: Arc<RuntimeTelemetry>,
        log_path: Option<PathBuf>,
        mut shutdown: watch::Receiver<bool>,
    ) {
        let mut audit_log = P2pAuditLogger::new(log_path);
        let existing = pool
            .pooled_transactions()
            .into_iter()
            .map(|tx| pending_tx_from_reth(&tx))
            .collect::<Vec<_>>();

        telemetry.record_backfill_size(existing.len());
        audit_log.log(
            "EL",
            "INF",
            "pool",
            &format!("BackfillStart count={}", existing.len()),
        );

        for tx in existing {
            tokio::select! {
                _ = shutdown.changed() => break,
                else => {
                    audit_log.log("EL", "OUT", "tracker", &format!("BackfillTx {:?}", tx.id));
                    if event_tx.send(MempoolEvent::PendingTransaction(tx)).is_err() {
                        break;
                    }
                }
            }
        }
    }

    fn pending_tx_from_signed(tx: &TransactionSigned) -> Option<PendingTx> {
        let sender = tx.recover_signer().ok()?;
        Some(PendingTx {
            id: ExecutionHash::from(tx.tx_hash().0),
            sender: Address(sender.0.0),
            nonce: tx.nonce(),
            max_fee_per_gas: tx.max_fee_per_gas(),
            max_priority_fee_per_gas: tx.max_priority_fee_per_gas().unwrap_or_default(),
            gas_limit: tx.gas_limit(),
            seen_at: std::time::SystemTime::now(),
        })
    }

    fn pending_tx_from_reth(
        tx: &reth_ethereum::pool::ValidPoolTransaction<EthPooledTransaction>,
    ) -> PendingTx {
        PendingTx {
            id: ExecutionHash::from(tx.hash().0),
            sender: Address(tx.sender().0.0),
            nonce: tx.nonce(),
            max_fee_per_gas: tx.max_fee_per_gas(),
            max_priority_fee_per_gas: tx
                .transaction
                .max_priority_fee_per_gas()
                .unwrap_or_else(|| tx.priority_fee_or_price()),
            gas_limit: tx.gas_limit(),
            seen_at: std::time::SystemTime::now(),
        }
    }

    #[cfg(feature = "consensus-p2p")]
    mod consensus {
        use super::*;
        use alloy::eips::eip2718::Decodable2718;
        use eth2_libp2p::{
            Context, Enr, MessageAcceptance, NetworkConfig as ConsensusNetworkConfig, NetworkEvent,
            PeerId, PubsubMessage, Response,
            libp2p::identity::secp256k1,
            rpc::methods::OldBlocksByRangeRequest,
            rpc::{RequestType, StatusMessage, StatusMessageV2},
            service::Network as ConsensusNetwork,
            service::api_types::{self, AppRequestId},
            types::{EnrForkId, ForkContext, GossipKind},
        };
        use grandine_types::{
            combined::{
                ExecutionPayload as CombinedExecutionPayload,
                SignedBeaconBlock as CombinedSignedBeaconBlock,
            },
            config::Config as ChainConfig,
            nonstandard::Phase,
            phase0::{consts::FAR_FUTURE_EPOCH, primitives::H256},
            preset::Mainnet,
            traits::SignedBeaconBlock as _,
        };
        use std::{
            collections::{BTreeMap, HashMap, HashSet},
            fs,
            net::Ipv4Addr,
            sync::Arc,
        };

        const MAINNET_GENESIS_VALIDATORS_ROOT: H256 = H256([
            0x4b, 0x36, 0x3d, 0xb9, 0x4e, 0x28, 0x61, 0x20, 0xd7, 0x6e, 0xb9, 0x05, 0x34, 0x0f,
            0xdd, 0x4e, 0x54, 0xbf, 0xe9, 0xf0, 0x6b, 0xf3, 0x3f, 0xf6, 0xcf, 0x5a, 0xd2, 0x7f,
            0x51, 0x1b, 0xfe, 0x95,
        ]);

        const MAINNET_CONSENSUS_BOOTNODES: &[&str] = &[
            "enr:-KG4QNTx85fjxABbSq_Rta9wy56nQ1fHK0PewJbGjLm1M4bMGx5-3Qq4ZX2-iFJ0pys_O90sVXNNOxp2E7afBsGsBrgDhGV0aDKQu6TalgMAAAD__________4JpZIJ2NIJpcIQEnfA2iXNlY3AyNTZrMaECGXWQ-rQ2KZKRH1aOW4IlPDBkY4XDphxg9pxKytFCkayDdGNwgiMog3VkcIIjKA",
            "enr:-KG4QF4B5WrlFcRhUU6dZETwY5ZzAXnA0vGC__L1Kdw602nDZwXSTs5RFXFIFUnbQJmhNGVU6OIX7KVrCSTODsz1tK4DhGV0aDKQu6TalgMAAAD__________4JpZIJ2NIJpcIQExNYEiXNlY3AyNTZrMaECQmM9vp7KhaXhI-nqL_R0ovULLCFSFTa9CPPSdb1zPX6DdGNwgiMog3VkcIIjKA",
            "enr:-Iu4QCV0e-_1Uw7p5mwRgx02z2zxnCGXCrWaBZspT0bZT6kcdA9nkWTHRsz2zt09SB2QJ46qhNjOKzQPMcz6MH1pq3MLY26CaWSCdjSCaXCEwiErIHDDAgIBiXNlY3AyNTZrMaEDF0wfAJ-f1UZtpG7RdNSiVhjDl_ktP1dsDioUcGO2f1ODdWRwgiOM",
            "enr:-Iu4QHs9DjoZ6gJHeOba6GbjXVl212tQsfX0TWrNeIXDLt42HHh8shfpUIzEZLSdnH9PIMox24uAYgmh4BAkhbb1_34LY26CaWSCdjSCaXCEwiErIXDDAgIBiXNlY3AyNTZrMaEDjj_JhExvBxl-vod_kHHqwBTJImdUAaxxOs1Sq6tE_4WDdWRwgiOM",
            "enr:-Ku4QImhMc1z8yCiNJ1TyUxdcfNucje3BGwEHzodEZUan8PherEo4sF7pPHPSIB1NNuSg5fZy7qFsjmUKs2ea1Whi0EBh2F0dG5ldHOIAAAAAAAAAACEZXRoMpD1pf1CAAAAAP__________gmlkgnY0gmlwhBLf22SJc2VjcDI1NmsxoQOVphkDqal4QzPMksc5wnpuC3gvSC8AfbFOnZY_On34wIN1ZHCCIyg",
        ];

        #[derive(Clone, Debug)]
        struct ObservedBlock {
            number: u64,
            hash: B256,
            slot: u64,
            update: BlockUpdate,
        }

        struct RecoveryChunk {
            sent_at: std::time::Instant,
        }

        struct ConsensusState {
            connected_peers: HashSet<PeerId>,
            peer_statuses: HashMap<PeerId, StatusMessage>,
            peer_directions: HashMap<PeerId, bool>,
            pending_status_requests: HashMap<PeerId, std::time::Instant>,
            pending_recovery_chunks: HashMap<api_types::Id, RecoveryChunk>,
            slot_to_number: HashMap<u64, u64>,
            recovery_target_number: Option<u64>,
            recovery_target_block: Option<ObservedBlock>,
            last_emitted_number: Option<u64>,
            last_emitted_slot: Option<u64>,
            buffered: BTreeMap<u64, ObservedBlock>,
            next_request_id: api_types::Id,
            network_handle:
                reth_ethereum::network::NetworkHandle<reth_ethereum::network::EthNetworkPrimitives>,
            audit_log: P2pAuditLogger,
        }

        impl ConsensusState {
            fn new(
                network_handle: reth_ethereum::network::NetworkHandle<
                    reth_ethereum::network::EthNetworkPrimitives,
                >,
                log_path: Option<std::path::PathBuf>,
            ) -> Self {
                Self {
                    connected_peers: HashSet::new(),
                    peer_statuses: HashMap::new(),
                    peer_directions: HashMap::new(),
                    pending_status_requests: HashMap::new(),
                    pending_recovery_chunks: HashMap::new(),
                    slot_to_number: HashMap::new(),
                    recovery_target_number: None,
                    recovery_target_block: None,
                    last_emitted_number: None,
                    last_emitted_slot: None,
                    buffered: BTreeMap::new(),
                    next_request_id: 1,
                    network_handle,
                    audit_log: P2pAuditLogger::new(log_path),
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
            network_handle: reth_ethereum::network::NetworkHandle<
                reth_ethereum::network::EthNetworkPrimitives,
            >,
            mut shutdown: watch::Receiver<bool>,
        ) {
            let chain_config = Arc::new(ChainConfig::mainnet());
            let network_dir = consensus_network_dir();

            if fs::create_dir_all(&network_dir).is_err() {
                return;
            }

            let mut consensus_config = ConsensusNetworkConfig::default();
            consensus_config.network_dir = Some(network_dir);

            if let Some(port) = config.consensus_port {
                consensus_config.set_ipv4_listening_address(
                    Ipv4Addr::UNSPECIFIED,
                    port,
                    port,
                    port,
                );
            }

            for node in &config.bootnodes {
                if let Ok(enr) = Enr::from_str(node) {
                    consensus_config.boot_nodes_enr.push(enr);
                } else if let Ok(addr) = eth2_libp2p::Multiaddr::from_str(node) {
                    consensus_config.boot_nodes_multiaddr.push(addr);
                }
            }

            if consensus_config.boot_nodes_enr.is_empty()
                && consensus_config.boot_nodes_multiaddr.is_empty()
            {
                for node in MAINNET_CONSENSUS_BOOTNODES {
                    if let Ok(enr) = Enr::from_str(node) {
                        consensus_config.boot_nodes_enr.push(enr);
                    }
                }
            }

            if !config.discovery_v4 {
                consensus_config.disable_discovery = true;
            }

            let (shutdown_tx, _) = futures_mpsc::channel(1);
            let executor = eth2_libp2p::TaskExecutor::new(shutdown_tx);

            let genesis_time = 1606824023; // Mainnet Genesis
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();
            let current_slot = if now > genesis_time {
                (now - genesis_time) / 12
            } else {
                0
            };

            let fork_context = Arc::new(ForkContext::new::<Mainnet>(
                &chain_config,
                current_slot,
                MAINNET_GENESIS_VALIDATORS_ROOT,
            ));
            let custody_group_count = chain_config.custody_requirement;
            let enr_fork_id = EnrForkId {
                fork_digest: fork_context.current_fork_digest(),
                next_fork_version: chain_config.version(latest_enabled_phase(&chain_config)),
                next_fork_epoch: FAR_FUTURE_EPOCH,
            };
            let context = Context {
                chain_config: chain_config.clone(),
                config: Arc::new(consensus_config),
                enr_fork_id,
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
            .await
            else {
                return;
            };

            service.subscribe_kind(GossipKind::BeaconBlock);
            service.subscribe_kind(GossipKind::LightClientFinalityUpdate);

            let mut state = ConsensusState::new(network_handle, config.log_path);

            loop {
                tokio::select! {
                    _ = shutdown.changed() => break,
                    event = service.next_event() => {
                        if !handle_network_event(&mut service, &mut state, &pool, &event_tx, &telemetry, &fork_context, event).await {
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
                NetworkEvent::PeerConnectedIncoming(peer_id) => {
                    state
                        .audit_log
                        .log("CL", "IN ", &peer_id.to_string(), "Connected");
                    telemetry.record_peer_event(
                        "CL",
                        "IN",
                        peer_id.to_string(),
                        "Connected",
                        String::new(),
                    );
                    state.connected_peers.insert(peer_id);
                    state.peer_directions.insert(peer_id, true);
                    telemetry.record_cl_connection(peer_id.to_string(), true);
                    telemetry.record_consensus_peer_count(state.connected_peers.len());
                }
                NetworkEvent::PeerConnectedOutgoing(peer_id) => {
                    state
                        .audit_log
                        .log("CL", "OUT", &peer_id.to_string(), "Connected");
                    telemetry.record_peer_event(
                        "CL",
                        "OUT",
                        peer_id.to_string(),
                        "Connected",
                        String::new(),
                    );
                    state.connected_peers.insert(peer_id);
                    state.peer_directions.insert(peer_id, false);
                    telemetry.record_cl_connection(peer_id.to_string(), false);
                    telemetry.record_consensus_peer_count(state.connected_peers.len());
                }
                NetworkEvent::PeerDisconnected(peer_id) => {
                    state
                        .audit_log
                        .log("CL", "END", &peer_id.to_string(), "Disconnected");
                    telemetry.record_peer_event(
                        "CL",
                        "END",
                        peer_id.to_string(),
                        "Disconnected",
                        String::new(),
                    );
                    state.connected_peers.remove(&peer_id);
                    state.peer_statuses.remove(&peer_id);
                    if let Some(is_ingress) = state.peer_directions.remove(&peer_id) {
                        telemetry.record_cl_disconnection(is_ingress);
                    }
                    telemetry.record_consensus_peer_count(state.connected_peers.len());
                }
                NetworkEvent::StatusPeer(peer_id) => {
                    state
                        .audit_log
                        .log("CL", "OUT", &peer_id.to_string(), "StatusRequest");
                    telemetry.record_peer_event(
                        "CL",
                        "OUT",
                        peer_id.to_string(),
                        "StatusRequest",
                        String::new(),
                    );
                    telemetry.record_cl_status_sent();
                    state
                        .pending_status_requests
                        .insert(peer_id, std::time::Instant::now());
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
                    state.audit_log.log(
                        "CL",
                        "IN ",
                        &peer_id.to_string(),
                        &format!("StatusRequest {remote:?}"),
                    );
                    telemetry.record_peer_event(
                        "CL",
                        "IN",
                        peer_id.to_string(),
                        "StatusRequest",
                        format!("{remote:?}"),
                    );
                    telemetry.record_cl_status_received(0);
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
                    state.audit_log.log(
                        "CL",
                        "IN ",
                        &peer_id.to_string(),
                        &format!("StatusResponse {status:?}"),
                    );
                    telemetry.record_peer_event(
                        "CL",
                        "IN",
                        peer_id.to_string(),
                        "StatusResponse",
                        format!("{status:?}"),
                    );
                    let latency = state
                        .pending_status_requests
                        .remove(&peer_id)
                        .map(|sent| sent.elapsed().as_nanos() as u64)
                        .unwrap_or(0);
                    telemetry.record_cl_status_received(latency);
                    state.peer_statuses.insert(peer_id, status);
                }
                NetworkEvent::PubsubMessage {
                    id,
                    source,
                    message: PubsubMessage::BeaconBlock(block),
                    ..
                } => {
                    service.report_message_validation_result(
                        &source,
                        id,
                        MessageAcceptance::Accept,
                    );
                    state
                        .audit_log
                        .log("CL", "IN ", &source.to_string(), "BeaconBlock");
                    telemetry.record_cl_block_received();
                    if let Some(observed) = observed_block_from_beacon_block(block, source) {
                        telemetry.record_consensus_last_block(BlockNumber(observed.number));
                        if !ingest_block(service, state, pool, event_tx, telemetry, observed).await
                        {
                            return false;
                        }
                    }
                }
                NetworkEvent::PubsubMessage {
                    id,
                    source,
                    message: PubsubMessage::LightClientFinalityUpdate(update),
                    ..
                } => {
                    service.report_message_validation_result(
                        &source,
                        id,
                        MessageAcceptance::Accept,
                    );
                    state
                        .audit_log
                        .log("CL", "IN ", &source.to_string(), "FinalityUpdate");
                    telemetry.record_cl_finality_update_received();
                    let finalized_slot = match *update {
                        grandine_types::combined::LightClientFinalityUpdate::Altair(u) => {
                            u.finalized_header.beacon.slot
                        }
                        grandine_types::combined::LightClientFinalityUpdate::Capella(u) => {
                            u.finalized_header.beacon.slot
                        }
                        grandine_types::combined::LightClientFinalityUpdate::Deneb(u) => {
                            u.finalized_header.beacon.slot
                        }
                        grandine_types::combined::LightClientFinalityUpdate::Electra(u) => {
                            u.finalized_header.beacon.slot
                        }
                        grandine_types::combined::LightClientFinalityUpdate::Fulu(u) => {
                            u.finalized_header.beacon.slot
                        }
                        grandine_types::combined::LightClientFinalityUpdate::Gloas(u) => {
                            u.finalized_header.beacon.slot
                        }
                    };
                    telemetry.record_consensus_finalized_slot(Slot(finalized_slot));

                    // The finalized slot corresponds to a block number we should have seen.
                    // If we haven't seen it yet, we might still be catching up, but once we do,
                    // the depth calculation will become accurate.
                    if let Some(&finalized_number) = state.slot_to_number.get(&finalized_slot) {
                        telemetry.record_consensus_finalized_number(BlockNumber(finalized_number));
                    }
                    let _ = event_tx.send(MempoolEvent::FinalizedBlock(Slot(finalized_slot)));
                }
                NetworkEvent::ResponseReceived {
                    peer_id,
                    response: Response::BlocksByRange(block),
                    app_request_id: AppRequestId::Application(request_id),
                    ..
                } => {
                    state.audit_log.log(
                        "CL",
                        "IN ",
                        &peer_id.to_string(),
                        &format!(
                            "BlocksByRangeResponse id={request_id} count={}",
                            block.is_some() as usize
                        ),
                    );
                    telemetry.record_peer_event(
                        "CL",
                        "IN",
                        peer_id.to_string(),
                        "BlocksByRangeResponse",
                        format!("id={request_id} count={}", block.is_some() as usize),
                    );

                    if let Some(chunk) = state.pending_recovery_chunks.get(&request_id) {
                        let latency = chunk.sent_at.elapsed().as_nanos() as u64;
                        telemetry.record_cl_blocks_by_range_response_received(latency);
                    }

                    match block {
                        Some(block) => {
                            if let Some(observed) = observed_block_from_beacon_block(block, peer_id)
                            {
                                if let Some(last) = state.last_emitted_number {
                                    if observed.number > last {
                                        state.buffered.entry(observed.number).or_insert(observed);
                                    }
                                }
                            }
                        }
                        None => {
                            state.pending_recovery_chunks.remove(&request_id);
                            if state.pending_recovery_chunks.is_empty() {
                                if !drain_buffered_blocks(state, event_tx, telemetry).await {
                                    return false;
                                }
                                if let Some(target) = state.recovery_target_number {
                                    if state.last_emitted_number.is_some_and(|n| n >= target) {
                                        state.recovery_target_number = None;
                                        state.recovery_target_block = None;
                                    }
                                }
                            }
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

            state
                .buffered
                .entry(observed.number)
                .or_insert(observed.clone());

            if state.pending_recovery_chunks.is_empty() {
                let last_slot = state.last_emitted_slot.unwrap_or(observed.slot);
                let slot_gap = observed.slot.saturating_sub(last_slot);

                if slot_gap == 0 || slot_gap > 1024 {
                    return reset_with_anchor(state, pool, event_tx, telemetry, observed).await;
                }

                state.recovery_target_number = Some(observed.number);
                state.recovery_target_block = Some(observed.clone());

                let available_peers: Vec<PeerId> = state.connected_peers.iter().cloned().collect();
                if available_peers.is_empty() {
                    return reset_with_anchor(state, pool, event_tx, telemetry, observed).await;
                }

                const MAX_CHUNK_SIZE: u64 = 128;
                let mut current_slot = last_slot + 1;
                let mut remaining_count = slot_gap;
                let mut peer_idx = 0;

                while remaining_count > 0 {
                    let count = remaining_count.min(MAX_CHUNK_SIZE);
                    let peer_id = available_peers[peer_idx % available_peers.len()];
                    peer_idx += 1;

                    let request_id = match state.next_app_request_id() {
                        AppRequestId::Application(id) => id,
                        _ => unreachable!(),
                    };

                    let request = RequestType::BlocksByRange(OldBlocksByRangeRequest::new(
                        current_slot,
                        count,
                        1,
                    ));
                    state.audit_log.log(
                        "CL",
                        "OUT",
                        &peer_id.to_string(),
                        &format!(
                            "BlocksByRangeRequest id={request_id} from={current_slot} count={count}"
                        ),
                    );
                    telemetry.record_peer_event(
                        "CL",
                        "OUT",
                        peer_id.to_string(),
                        "BlocksByRangeRequest",
                        format!("id={request_id} from={current_slot} count={count}"),
                    );
                    telemetry.record_cl_blocks_by_range_request_sent();

                    if service
                        .send_request(peer_id, AppRequestId::Application(request_id), request)
                        .is_ok()
                    {
                        state.pending_recovery_chunks.insert(
                            request_id,
                            RecoveryChunk {
                                sent_at: std::time::Instant::now(),
                            },
                        );
                    }
                    current_slot += count;
                    remaining_count -= count;
                }
            }
            true
        }

        async fn drain_buffered_blocks(
            state: &mut ConsensusState,
            event_tx: &mpsc::Sender<MempoolEvent>,
            telemetry: &Arc<RuntimeTelemetry>,
        ) -> bool {
            while let Some(last) = state.last_emitted_number {
                let next = last + 1;
                if let Some(observed) = state.buffered.remove(&next) {
                    if !emit_block(state, event_tx, telemetry, observed).await {
                        return false;
                    }
                } else {
                    break;
                }
            }
            true
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

            state.network_handle.update_status(Head {
                number: block_number,
                hash: block.hash,
                difficulty: U256::ZERO,
                total_difficulty: U256::ZERO,
                timestamp: 0,
            });

            if event_tx.send(MempoolEvent::NewBlock(block.update)).is_err() {
                return false;
            }

            telemetry.record_block(tx_count, gas_used);
            state.last_emitted_number = Some(block_number);
            state.last_emitted_slot = Some(block.slot);
            state.slot_to_number.insert(block.slot, block_number);
            if state.slot_to_number.len() > 1024 {
                let oldest = block.slot.saturating_sub(1024);
                state.slot_to_number.retain(|&s, _| s > oldest);
            }
            true
        }

        async fn emit_anchor(
            state: &mut ConsensusState,
            _pool: &EmbeddedPool,
            event_tx: &mpsc::Sender<MempoolEvent>,
            telemetry: &Arc<RuntimeTelemetry>,
            anchor: ObservedBlock,
        ) -> bool {
            let future_blocks = state.buffered.values().cloned().map(|b| b.update).collect();
            if event_tx
                .send(MempoolEvent::Prune(TrackerPrune {
                    anchor: anchor.update.clone(),
                    future_blocks,
                }))
                .is_err()
            {
                return false;
            }

            telemetry.record_consensus_anchor(BlockNumber(anchor.number));
            telemetry.record_consensus_last_block(BlockNumber(anchor.number));
            telemetry.record_consensus_next_expected(BlockNumber(anchor.number + 1));
            telemetry.record_block(anchor.update.included_txs.len(), anchor.update.gas_used);

            state.last_emitted_number = Some(anchor.number);
            state.last_emitted_slot = Some(anchor.slot);
            state.slot_to_number.insert(anchor.slot, anchor.number);
            state.pending_recovery_chunks.clear();
            state.recovery_target_number = None;
            state.recovery_target_block = None;

            drain_buffered_blocks(state, event_tx, telemetry).await
        }

        async fn reset_with_anchor(
            state: &mut ConsensusState,
            pool: &EmbeddedPool,
            event_tx: &mpsc::Sender<MempoolEvent>,
            telemetry: &Arc<RuntimeTelemetry>,
            anchor: ObservedBlock,
        ) -> bool {
            telemetry.record_consensus_gap_reset();
            emit_anchor(state, pool, event_tx, telemetry, anchor).await
        }

        fn local_status(state: &ConsensusState, fork_context: &ForkContext) -> StatusMessage {
            let head_slot = state.last_emitted_slot.unwrap_or_default();
            let earliest_available_slot = state.last_emitted_slot.unwrap_or_default();
            StatusMessage::V2(StatusMessageV2 {
                fork_digest: fork_context.current_fork_digest(),
                finalized_root: MAINNET_GENESIS_VALIDATORS_ROOT,
                finalized_epoch: 0,
                head_root: MAINNET_GENESIS_VALIDATORS_ROOT,
                head_slot,
                earliest_available_slot,
            })
        }

        fn observed_block_from_beacon_block(
            block: Arc<CombinedSignedBeaconBlock<Mainnet>>,
            _source: PeerId,
        ) -> Option<ObservedBlock> {
            let slot = block.message().slot();
            let payload = block.as_ref().clone().execution_payload()?;
            let (number, hash, parent_hash, gas_used, gas_limit, new_base_fee, included_txs) =
                match payload {
                    CombinedExecutionPayload::Bellatrix(payload) => (
                        payload.block_number,
                        B256::from_slice(payload.block_hash.as_bytes()),
                        B256::from_slice(payload.parent_hash.as_bytes()),
                        payload.gas_used,
                        payload.gas_limit,
                        payload
                            .base_fee_per_gas
                            .into_raw()
                            .try_into()
                            .unwrap_or(u128::MAX),
                        decode_payload_transactions(payload.transactions.iter()),
                    ),
                    CombinedExecutionPayload::Capella(payload) => (
                        payload.block_number,
                        B256::from_slice(payload.block_hash.as_bytes()),
                        B256::from_slice(payload.parent_hash.as_bytes()),
                        payload.gas_used,
                        payload.gas_limit,
                        payload
                            .base_fee_per_gas
                            .into_raw()
                            .try_into()
                            .unwrap_or(u128::MAX),
                        decode_payload_transactions(payload.transactions.iter()),
                    ),
                    CombinedExecutionPayload::Deneb(payload) => (
                        payload.block_number,
                        B256::from_slice(payload.block_hash.as_bytes()),
                        B256::from_slice(payload.parent_hash.as_bytes()),
                        payload.gas_used,
                        payload.gas_limit,
                        payload
                            .base_fee_per_gas
                            .into_raw()
                            .try_into()
                            .unwrap_or(u128::MAX),
                        decode_payload_transactions(payload.transactions.iter()),
                    ),
                };

            Some(ObservedBlock {
                number,
                hash,
                slot,
                update: BlockUpdate {
                    number: BlockNumber(number),
                    hash: ExecutionHash::from(hash.0),
                    parent_hash: ExecutionHash::from(parent_hash.0),
                    included_txs,
                    new_base_fee,
                    gas_used,
                    gas_limit,
                },
            })
        }

        fn decode_payload_transactions<'a>(
            txs: impl Iterator<Item = &'a grandine_types::bellatrix::primitives::Transaction<Mainnet>>,
        ) -> Vec<PendingTx> {
            txs.filter_map(|tx_bytes| {
                let tx = TransactionSigned::decode_2718(&mut tx_bytes.as_bytes()).ok()?;
                pending_tx_from_signed(&tx)
            })
            .collect()
        }

        fn latest_enabled_phase(config: &ChainConfig) -> Phase {
            if config.fulu_fork_epoch != FAR_FUTURE_EPOCH {
                Phase::Fulu
            } else if config.deneb_fork_epoch != FAR_FUTURE_EPOCH {
                Phase::Deneb
            } else if config.capella_fork_epoch != FAR_FUTURE_EPOCH {
                Phase::Capella
            } else if config.bellatrix_fork_epoch != FAR_FUTURE_EPOCH {
                Phase::Bellatrix
            } else if config.altair_fork_epoch != FAR_FUTURE_EPOCH {
                Phase::Altair
            } else {
                Phase::Phase0
            }
        }

        fn consensus_network_dir() -> PathBuf {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis();
            std::env::temp_dir().join(format!("mempooloracle-consensus-{unique}"))
        }
    }
}

#[cfg(feature = "reth-p2p")]
pub use enabled::connect_with_config;
