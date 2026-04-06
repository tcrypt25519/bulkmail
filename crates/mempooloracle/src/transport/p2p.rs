#[cfg(feature = "reth-p2p")]
mod enabled {
    #[cfg(feature = "consensus-p2p")]
    type ConsensusNetworkEvent =
        eth2_libp2p::NetworkEvent<grandine_types::preset::Mainnet>;

    use crate::{
        Address, BlockUpdate, ConsensusTransportImplementation, MempoolEvent, MempoolTracker,
        P2pBlockTransport, P2pTransportConfig, PendingTx, TrackerConfig, TrackerError,
        TrackerRuntime, TxId,
        runtime::{DEFAULT_SHUTDOWN_VALUE, RuntimeTelemetry, TransportKind},
    };
    use alloy::{
        consensus::{BlockHeader as _, Transaction as _},
        eips::BlockHashOrNumber,
    };
    use futures::StreamExt;
    use reth_ethereum::{
        BlockBody, TransactionSigned,
        network::{
            EthNetworkPrimitives, NetworkConfig, NetworkEvent, NetworkEventListenerProvider,
            NetworkManager, PeerRequest, PeerRequestSender, config::rng_secret_key,
            eth_wire::{GetBlockBodies, GetBlockHeaders, HeadersDirection},
            events::PeerEvent,
        },
        pool::{
            CoinbaseTipOrdering, EthPooledTransaction, Pool, TransactionListenerKind,
            TransactionPool, blobstore::InMemoryBlobStore, test_utils::OkValidator,
        },
        provider::test_utils::NoopProvider,
        primitives::SignerRecoverable as _,
    };
    use reth_network_peers::TrustedPeer;
    use std::{
        collections::HashMap,
        str::FromStr,
        sync::{Arc, Mutex, mpsc},
        thread,
    };
    use tokio::{
        sync::{oneshot, watch},
        time::{self, Duration},
    };

    const BLOCK_POLL_INTERVAL: Duration = Duration::from_secs(2);
    const PEER_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
    const MAX_HEADERS_PER_POLL: u64 = 8;

    type EmbeddedPool = Pool<
        OkValidator<EthPooledTransaction>,
        CoinbaseTipOrdering<EthPooledTransaction>,
        InMemoryBlobStore,
    >;

    type PeerSessions =
        Arc<Mutex<HashMap<reth_network_peers::PeerId, PeerSession<EthNetworkPrimitives>>>>;

    #[derive(Clone)]
    struct PeerSession<N: reth_ethereum::network::NetworkPrimitives> {
        latest_block: Option<u64>,
        head_hash: alloy::primitives::B256,
        requests: PeerRequestSender<PeerRequest<N>>,
    }

    pub async fn connect_with_config(
        config: P2pTransportConfig,
        tracker_config: TrackerConfig,
    ) -> Result<TrackerRuntime, TrackerError> {
        if config.chain != "mainnet" {
            return Err(TrackerError::UnsupportedTransport(
                "embedded reth p2p currently supports mainnet only",
            ));
        }

        match &config.block_transport {
            P2pBlockTransport::ExecutionPolling => {
                return Err(TrackerError::UnsupportedTransport(
                    "execution p2p block polling does not cover mainnet PoS head blocks; use a consensus block transport instead",
                ));
            }
            P2pBlockTransport::Consensus(consensus) => {
                #[cfg(feature = "consensus-p2p")]
                let _event_type_check: Option<ConsensusNetworkEvent> = None;
                let _implementation = match consensus.implementation {
                    ConsensusTransportImplementation::Eth2Libp2p => "eth2_libp2p",
                };
                #[cfg(not(feature = "consensus-p2p"))]
                return Err(TrackerError::FeatureDisabled("consensus-p2p"));
                return Err(TrackerError::UnsupportedTransport(
                    "consensus p2p block transport is not implemented yet",
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

        let local_key = rng_secret_key();
        let mut builder = NetworkConfig::<_, EthNetworkPrimitives>::builder(local_key);

        if let Some(addr) = config.listen_addr {
            builder = builder.set_addrs(addr);
        }

        if config.bootnodes.is_empty() {
            builder = builder.mainnet_boot_nodes();
        } else {
            let boot_nodes = parse_bootnodes(&config.bootnodes)?;
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
        let sessions = Arc::new(Mutex::new(HashMap::new()));

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
        let peer_events_task = tokio::spawn(run_peer_event_listener(
            network_handle.clone(),
            sessions.clone(),
            telemetry.clone(),
            shutdown_rx.clone(),
        ));
        let mut tasks = vec![network_task, txpool_task, pending_task, backfill_task, peer_events_task];

        if matches!(config.block_transport, P2pBlockTransport::ExecutionPolling) {
            tasks.push(tokio::spawn(run_block_poller(
                sessions,
                event_tx,
                telemetry.clone(),
                shutdown_rx,
            )));
        }

        Ok(TrackerRuntime::new(
            handle,
            telemetry,
            shutdown_tx,
            tasks,
        ))
    }

    fn parse_bootnodes(bootnodes: &[String]) -> Result<Vec<TrustedPeer>, TrackerError> {
        bootnodes
            .iter()
            .map(|node| {
                TrustedPeer::from_str(node).map_err(|err| {
                    TrackerError::Setup(format!("failed to parse bootnode `{node}`: {err}"))
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

    async fn run_peer_event_listener(
        network_handle: reth_ethereum::network::NetworkHandle<EthNetworkPrimitives>,
        sessions: PeerSessions,
        telemetry: Arc<RuntimeTelemetry>,
        mut shutdown: watch::Receiver<bool>,
    ) {
        let mut events = network_handle.event_listener();

        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                maybe_event = events.next() => {
                    let Some(event) = maybe_event else {
                        break;
                    };

                    match event {
                        NetworkEvent::ActivePeerSession { info, messages } => {
                            let peer_id = info.peer_id;
                            let session = PeerSession {
                                latest_block: info.status.latest_block,
                                head_hash: info.status.blockhash,
                                requests: messages,
                            };

                            let peer_count = {
                                let mut guard = sessions.lock().expect("peer sessions lock poisoned");
                                guard.insert(peer_id, session);
                                guard.len()
                            };
                            telemetry.record_p2p_peer_count(peer_count);
                        }
                        NetworkEvent::Peer(PeerEvent::SessionClosed { peer_id, .. }) |
                        NetworkEvent::Peer(PeerEvent::PeerRemoved(peer_id)) => {
                            let peer_count = {
                                let mut guard = sessions.lock().expect("peer sessions lock poisoned");
                                guard.remove(&peer_id);
                                guard.len()
                            };
                            telemetry.record_p2p_peer_count(peer_count);
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    async fn run_block_poller(
        sessions: PeerSessions,
        event_tx: mpsc::Sender<MempoolEvent>,
        telemetry: Arc<RuntimeTelemetry>,
        mut shutdown: watch::Receiver<bool>,
    ) {
        let mut interval = time::interval(BLOCK_POLL_INTERVAL);
        let mut next_block_number = None;
        let mut last_block_hash = None;

        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                _ = interval.tick() => {}
            }

            let Some(peer) = select_peer(&sessions, next_block_number) else {
                continue;
            };

            let headers = match request_next_headers(&peer, next_block_number).await {
                Ok(headers) if !headers.is_empty() => headers,
                _ => continue,
            };

            for header in headers {
                let header_hash = header.hash_slow();
                if last_block_hash == Some(header_hash) {
                    next_block_number = Some(header.number() + 1);
                    continue;
                }

                let Some(body) = request_block_body(&peer.requests, header_hash).await else {
                    break;
                };

                let block_update = block_update_from_header_body(&header, &body);
                telemetry.record_block(block_update.included_txs.len(), block_update.gas_used);

                if event_tx.send(MempoolEvent::NewBlock(block_update)).is_err() {
                    return;
                }

                last_block_hash = Some(header_hash);
                next_block_number = Some(header.number() + 1);
            }
        }
    }

    fn select_peer(
        sessions: &PeerSessions,
        next_block_number: Option<u64>,
    ) -> Option<PeerSession<EthNetworkPrimitives>> {
        let guard = sessions.lock().expect("peer sessions lock poisoned");

        if let Some(target) = next_block_number {
            guard
                .values()
                .filter(|session| session.latest_block.is_none_or(|latest| latest + 1 >= target))
                .cloned()
                .max_by_key(|session| session.latest_block.unwrap_or_default())
                .or_else(|| guard.values().next().cloned())
        } else {
            guard
                .values()
                .cloned()
                .max_by_key(|session| session.latest_block.unwrap_or_default())
        }
    }

    async fn request_next_headers(
        peer: &PeerSession<EthNetworkPrimitives>,
        next_block_number: Option<u64>,
    ) -> Result<Vec<alloy::consensus::Header>, TrackerError> {
        let request = if let Some(number) = next_block_number {
            GetBlockHeaders {
                start_block: BlockHashOrNumber::Number(number),
                limit: MAX_HEADERS_PER_POLL,
                skip: 0,
                direction: HeadersDirection::Rising,
            }
        } else if let Some(number) = peer.latest_block {
            GetBlockHeaders {
                start_block: BlockHashOrNumber::Number(number),
                limit: 1,
                skip: 0,
                direction: HeadersDirection::Rising,
            }
        } else {
            GetBlockHeaders {
                start_block: BlockHashOrNumber::Hash(peer.head_hash),
                limit: 1,
                skip: 0,
                direction: HeadersDirection::Rising,
            }
        };

        let (response_tx, response_rx) = oneshot::channel();
        peer.requests
            .to_session_tx
            .send(PeerRequest::GetBlockHeaders {
                request,
                response: response_tx,
            })
            .await
            .map_err(|err| TrackerError::Setup(format!("failed to request block headers: {err}")))?;

        let response = time::timeout(PEER_REQUEST_TIMEOUT, response_rx)
            .await
            .map_err(|_| TrackerError::Timeout {
                stage: "p2p getBlockHeaders",
            })?
            .map_err(|err| TrackerError::Setup(format!("header response channel dropped: {err}")))?
            .map_err(|err| TrackerError::Setup(format!("block header request failed: {err}")))?;

        Ok(response.0)
    }

    async fn request_block_body(
        requests: &PeerRequestSender<PeerRequest<EthNetworkPrimitives>>,
        block_hash: alloy::primitives::B256,
    ) -> Option<BlockBody> {
        let (response_tx, response_rx) = oneshot::channel();
        requests
            .to_session_tx
            .send(PeerRequest::GetBlockBodies {
                request: GetBlockBodies(vec![block_hash]),
                response: response_tx,
            })
            .await
            .ok()?;

        let response = time::timeout(PEER_REQUEST_TIMEOUT, response_rx).await.ok()?;
        let response = response.ok()?.ok()?;
        response.0.into_iter().next()
    }

    fn block_update_from_header_body(
        header: &alloy::consensus::Header,
        body: &BlockBody,
    ) -> BlockUpdate {
        let included_txs = body
            .transactions
            .iter()
            .filter_map(pending_tx_from_signed)
            .collect::<Vec<_>>();

        let new_base_fee = header
            .next_block_base_fee(alloy::eips::eip1559::BaseFeeParams::ethereum())
            .or_else(|| header.base_fee_per_gas())
            .unwrap_or_default() as u128;

        BlockUpdate {
            included_txs,
            new_base_fee,
            gas_used: header.gas_used(),
            gas_limit: header.gas_limit(),
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
