use std::net::IpAddr;
use std::time::Instant;
use std::{ops::Neg, sync::Arc};

use chunk_pool::ChunkPoolMessage;
use file_location_cache::FileLocationCache;
use network::multiaddr::Protocol;
use network::types::TimedMessage;
use network::{
    rpc::StatusMessage,
    types::{
        AnnounceChunks, AnnounceFile, FindChunks, FindFile, HasSignature, SignedAnnounceFile,
        SignedMessage,
    },
    Keypair, MessageAcceptance, MessageId, NetworkGlobals, NetworkMessage, PeerId, PeerRequestId,
    PublicKey, PubsubMessage, Request, RequestId, Response,
};
use network::{Multiaddr, NetworkSender, PeerAction, ReportSource};
use shared_types::{bytes_to_chunks, timestamp_now, NetworkIdentity, ShardedFile, TxID};
use storage::config::ShardConfig;
use storage_async::Store;
use sync::{SyncMessage, SyncSender};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::{mpsc, RwLock};

use crate::batcher::Batcher;
use crate::metrics::{self, PubsubMsgHandleMetrics};
use crate::peer_manager::PeerManager;
use crate::Config;

lazy_static::lazy_static! {
    /// Timeout to publish message to neighbor nodes.
    pub static ref PUBSUB_TIMEOUT_NEIGHBORS: chrono::Duration = chrono::Duration::seconds(30);
    /// Timeout to publish message to network.
    pub static ref PUBSUB_TIMEOUT_NETWORK: chrono::Duration = chrono::Duration::minutes(5);
    pub static ref TOLERABLE_DRIFT: chrono::Duration = chrono::Duration::seconds(10);
}

fn duration_since(timestamp: u32, latency_ms: Arc<dyn ::metrics::Histogram>) -> chrono::Duration {
    let timestamp = i64::from(timestamp);
    let timestamp = chrono::DateTime::from_timestamp(timestamp, 0).expect("should fit");
    let now = chrono::Utc::now();
    let duration = now.signed_duration_since(timestamp);

    let num_millis = duration.num_milliseconds();
    if num_millis > 0 {
        latency_ms.update(num_millis as u64);
    }

    duration
}

impl PubsubMsgHandleMetrics {
    pub fn verify_timestamp(
        &self,
        from: PeerId,
        timestamp: u32,
        timeout: chrono::Duration,
        sender: Option<&NetworkSender>,
    ) -> bool {
        self.qps.mark(1);

        let d = duration_since(timestamp, self.latency_ms.clone());
        if d >= TOLERABLE_DRIFT.neg() && d <= timeout {
            return true;
        }

        debug!(%from, ?timestamp, ?d, topic=%self.topic_name, "Ignore out of date pubsub message");

        self.timeout.mark(1);

        if let Some(sender) = sender {
            let _ = sender.send(NetworkMessage::ReportPeer {
                peer_id: from,
                action: PeerAction::LowToleranceError,
                source: ReportSource::Gossipsub,
                msg: "Received out of date pubsub message",
            });
        }

        false
    }
}

fn peer_id_to_public_key(peer_id: &PeerId) -> Result<PublicKey, String> {
    // A libp2p peer id byte representation should be 2 length bytes + 4 protobuf bytes + compressed pk bytes
    // if generated from a PublicKey with Identity multihash.
    let pk_bytes = &peer_id.to_bytes()[2..];

    PublicKey::from_protobuf_encoding(pk_bytes).map_err(|e| {
        format!(
            " Cannot parse libp2p public key public key from peer id: {}",
            e
        )
    })
}

fn verify_signature(msg: &dyn HasSignature, peer_id: &PeerId, propagation_source: PeerId) -> bool {
    match peer_id_to_public_key(peer_id) {
        Ok(pub_key) => msg.verify_signature(&pub_key),
        Err(err) => {
            error!(
                ?err,
                ?peer_id,
                ?propagation_source,
                "Failed to verify signature"
            );
            false
        }
    }
}

pub struct Libp2pEventHandler {
    config: Config,
    /// A collection of global variables, accessible outside of the network service.
    network_globals: Arc<NetworkGlobals>,
    /// A channel to the router service.
    network_send: NetworkSender,
    /// A channel to the syncing service.
    sync_send: SyncSender,
    /// A channel to the RPC chunk pool service.
    chunk_pool_send: mpsc::UnboundedSender<ChunkPoolMessage>,
    /// Node keypair for signing messages.
    local_keypair: Keypair,
    /// Log and transaction storage.
    store: Store,
    /// Cache for storing and serving gossip messages.
    file_location_cache: Arc<FileLocationCache>,
    /// All connected peers.
    peers: Arc<RwLock<PeerManager>>,
    /// Files to announce in batch
    file_batcher: RwLock<Batcher<TxID>>,
    /// Announcements to publish in batch
    announcement_batcher: RwLock<Batcher<SignedAnnounceFile>>,
}

impl Libp2pEventHandler {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: Config,
        network_globals: Arc<NetworkGlobals>,
        network_send: NetworkSender,
        sync_send: SyncSender,
        chunk_pool_send: UnboundedSender<ChunkPoolMessage>,
        local_keypair: Keypair,
        store: Store,
        file_location_cache: Arc<FileLocationCache>,
        peers: Arc<RwLock<PeerManager>>,
    ) -> Self {
        let file_batcher = RwLock::new(Batcher::new(
            config.batcher_file_capacity,
            config.batcher_timeout,
            "file",
        ));

        let announcement_batcher = RwLock::new(Batcher::new(
            config.batcher_announcement_capacity,
            config.batcher_timeout,
            "announcement",
        ));

        Self {
            config,
            network_globals,
            network_send,
            sync_send,
            chunk_pool_send,
            local_keypair,
            store,
            file_location_cache,
            peers,
            file_batcher,
            announcement_batcher,
        }
    }

    pub fn send_to_network(&self, message: NetworkMessage) {
        self.network_send.send(message).unwrap_or_else(|err| {
            warn!(%err, "Could not send message to the network service");
        });
    }

    pub fn send_to_sync(&self, message: SyncMessage) {
        self.sync_send.notify(message).unwrap_or_else(|err| {
            warn!(%err, "Could not send message to the sync service");
        });
    }

    pub fn send_to_chunk_pool(&self, message: ChunkPoolMessage) {
        self.chunk_pool_send.send(message).unwrap_or_else(|err| {
            warn!(%err, "Could not send message to the chunk pool service");
        });
    }

    pub fn publish(&self, msg: PubsubMessage) {
        self.send_to_network(NetworkMessage::Publish {
            messages: vec![msg],
        });
    }

    pub fn send_status(&self, peer_id: PeerId) {
        let shard_config = self.store.get_store().get_shard_config();
        let status_message = StatusMessage {
            data: self.network_globals.network_id(),
            num_shard: shard_config.num_shard,
            shard_id: shard_config.shard_id,
        };
        debug!(%peer_id, ?status_message, "Sending Status request");

        self.send_to_network(NetworkMessage::SendRequest {
            peer_id,
            request_id: RequestId::Router(Instant::now()),
            request: Request::Status(status_message),
        });

        metrics::LIBP2P_SEND_STATUS.mark(1);
    }

    pub async fn on_peer_connected(&self, peer_id: PeerId, outgoing: bool) {
        self.peers.write().await.add(peer_id, outgoing);

        if outgoing {
            self.send_status(peer_id);
            metrics::LIBP2P_HANDLE_PEER_CONNECTED_OUTGOING.mark(1);
        } else {
            metrics::LIBP2P_HANDLE_PEER_CONNECTED_INCOMING.mark(1);
        }
    }

    pub async fn on_peer_disconnected(&self, peer_id: PeerId) {
        self.peers.write().await.remove(&peer_id);
        self.send_to_sync(SyncMessage::PeerDisconnected { peer_id });
        metrics::LIBP2P_HANDLE_PEER_DISCONNECTED.mark(1);
    }

    pub async fn on_rpc_request(
        &self,
        peer_id: PeerId,
        request_id: PeerRequestId,
        request: Request,
    ) {
        self.peers.write().await.update(&peer_id);

        match request {
            Request::Status(status) => {
                self.on_status_request(peer_id, request_id, status);
                metrics::LIBP2P_HANDLE_STATUS_REQUEST.mark(1);
            }
            Request::GetChunks(request) => {
                self.send_to_sync(SyncMessage::RequestChunks {
                    peer_id,
                    request_id,
                    request,
                });
                metrics::LIBP2P_HANDLE_GET_CHUNKS_REQUEST.mark(1);
            }
            Request::AnswerFile(file) => match ShardConfig::try_from(file.shard_config) {
                Ok(v) => {
                    self.file_location_cache.insert_peer_config(peer_id, v);

                    self.send_to_sync(SyncMessage::AnswerFile { peer_id, file });
                }
                Err(_) => self.send_to_network(NetworkMessage::ReportPeer {
                    peer_id,
                    action: PeerAction::Fatal,
                    source: ReportSource::RPC,
                    msg: "Invalid shard config in AnswerFile RPC message",
                }),
            },
            Request::DataByHash(_) => {
                // ignore
            }
        }
    }

    fn on_status_request(&self, peer_id: PeerId, request_id: PeerRequestId, status: StatusMessage) {
        debug!(%peer_id, ?status, "Received Status request");

        let network_id = self.network_globals.network_id();
        let shard_config = self.store.get_store().get_shard_config();
        let status_message = StatusMessage {
            data: network_id.clone(),
            num_shard: shard_config.num_shard,
            shard_id: shard_config.shard_id,
        };
        debug!(%peer_id, ?status_message, "Sending Status response");

        self.send_to_network(NetworkMessage::SendResponse {
            peer_id,
            id: request_id,
            response: Response::Status(status_message),
        });

        if self.verify_status_message(peer_id, status, network_id, &shard_config) {
            self.send_to_sync(SyncMessage::PeerConnected { peer_id });
        }
    }

    fn on_status_response(&self, peer_id: PeerId, status: StatusMessage) {
        let network_id = self.network_globals.network_id();
        let shard_config = self.store.get_store().get_shard_config();
        if self.verify_status_message(peer_id, status, network_id, &shard_config) {
            self.send_to_sync(SyncMessage::PeerConnected { peer_id });
        }
    }

    pub async fn on_rpc_response(
        &self,
        peer_id: PeerId,
        request_id: RequestId,
        response: Response,
    ) {
        self.peers.write().await.update(&peer_id);

        match response {
            Response::Status(status_message) => {
                debug!(%peer_id, ?status_message, "Received Status response");
                match request_id {
                    RequestId::Router(since) => {
                        metrics::LIBP2P_HANDLE_STATUS_RESPONSE.mark(1);
                        metrics::LIBP2P_HANDLE_STATUS_RESPONSE_LATENCY.update_since(since);
                    }
                    _ => unreachable!("All status response belong to router"),
                }
                self.on_status_response(peer_id, status_message);
            }
            Response::Chunks(response) => {
                let request_id = match request_id {
                    RequestId::Sync(since, sync_id) => {
                        metrics::LIBP2P_HANDLE_GET_CHUNKS_RESPONSE.mark(1);
                        metrics::LIBP2P_HANDLE_GET_CHUNKS_RESPONSE_LATENCY.update_since(since);
                        sync_id
                    }
                    _ => unreachable!("All Chunks responses belong to sync"),
                };

                self.send_to_sync(SyncMessage::ChunksResponse {
                    peer_id,
                    request_id,
                    response,
                });
            }
            Response::DataByHash(_) => {
                // ignore
            }
        }
    }

    pub async fn on_rpc_error(&self, peer_id: PeerId, request_id: RequestId) {
        self.peers.write().await.update(&peer_id);

        // Check if the failed RPC belongs to sync
        if let RequestId::Sync(since, request_id) = request_id {
            self.send_to_sync(SyncMessage::RpcError {
                peer_id,
                request_id,
            });

            metrics::LIBP2P_HANDLE_RESPONSE_ERROR_LATENCY.update_since(since);
        }

        metrics::LIBP2P_HANDLE_RESPONSE_ERROR.mark(1);
    }

    pub async fn on_pubsub_message(
        &self,
        propagation_source: PeerId,
        source: PeerId,
        id: &MessageId,
        message: PubsubMessage,
    ) -> MessageAcceptance {
        trace!(?message, %propagation_source, %source, %id, "Received pubsub message");

        match message {
            PubsubMessage::ExampleMessage(_) => MessageAcceptance::Ignore,
            PubsubMessage::NewFile(msg) => self.on_new_file(propagation_source, msg).await,
            PubsubMessage::AskFile(msg) => self.on_ask_file(propagation_source, msg).await,
            PubsubMessage::FindFile(msg) => self.on_find_file(propagation_source, msg).await,
            PubsubMessage::FindChunks(msg) => self.on_find_chunks(propagation_source, msg).await,
            PubsubMessage::AnnounceFile(msgs) => {
                metrics::LIBP2P_HANDLE_PUBSUB_ANNOUNCE_FILE.mark(1);

                for msg in msgs {
                    match self.on_announce_file(propagation_source, msg) {
                        MessageAcceptance::Reject => return MessageAcceptance::Reject,
                        MessageAcceptance::Ignore => return MessageAcceptance::Ignore,
                        _ => {}
                    }
                }

                MessageAcceptance::Accept
            }
            PubsubMessage::AnnounceChunks(msg) => self.on_announce_chunks(propagation_source, msg),
            PubsubMessage::AnnounceShardConfig(msg) => {
                self.on_announce_shard_config(propagation_source, source, msg)
            }
        }
    }

    /// Handle NewFile pubsub message `msg` that published by `from` peer.
    async fn on_new_file(&self, from: PeerId, msg: TimedMessage<ShardedFile>) -> MessageAcceptance {
        // verify timestamp
        if !metrics::LIBP2P_HANDLE_PUBSUB_NEW_FILE.verify_timestamp(
            from,
            msg.timestamp,
            *PUBSUB_TIMEOUT_NEIGHBORS,
            Some(&self.network_send),
        ) {
            return MessageAcceptance::Ignore;
        }

        // verify announced shard config
        let announced_shard_config = match ShardConfig::try_from(msg.shard_config) {
            Ok(v) => v,
            Err(_) => return MessageAcceptance::Reject,
        };

        // ignore if shard config mismatch
        let my_shard_config = self.store.get_store().get_shard_config();
        if !my_shard_config.intersect(&announced_shard_config) {
            return MessageAcceptance::Ignore;
        }

        // ignore if already pruned or exists
        match self.store.get_store().get_tx_status(msg.tx_id.seq) {
            Ok(Some(_)) => return MessageAcceptance::Ignore,
            Ok(None) => {}
            Err(err) => {
                warn!(?err, tx_seq = %msg.tx_id.seq, "Failed to get tx status");
                return MessageAcceptance::Ignore;
            }
        }

        // notify sync layer to handle in advance
        self.send_to_sync(SyncMessage::NewFile {
            from,
            file: msg.inner,
        });

        MessageAcceptance::Ignore
    }

    async fn on_ask_file(&self, from: PeerId, msg: TimedMessage<ShardedFile>) -> MessageAcceptance {
        // verify timestamp
        if !metrics::LIBP2P_HANDLE_PUBSUB_ASK_FILE.verify_timestamp(
            from,
            msg.timestamp,
            *PUBSUB_TIMEOUT_NEIGHBORS,
            Some(&self.network_send),
        ) {
            return MessageAcceptance::Ignore;
        }

        // verify announced shard config
        let announced_shard_config = match ShardConfig::try_from(msg.shard_config) {
            Ok(v) => v,
            Err(_) => return MessageAcceptance::Reject,
        };

        // handle on shard config mismatch
        let my_shard_config = self.store.get_store().get_shard_config();
        if !my_shard_config.intersect(&announced_shard_config) {
            return MessageAcceptance::Ignore;
        }

        // check if we have it
        if matches!(self.store.check_tx_completed(msg.tx_id.seq).await, Ok(true)) {
            if let Ok(Some(tx)) = self.store.get_tx_by_seq_number(msg.tx_id.seq).await {
                if tx.id() == msg.tx_id {
                    trace!(?msg.tx_id, "Found file locally, responding to FindFile query");

                    self.send_to_network(NetworkMessage::SendRequest {
                        peer_id: from,
                        request: Request::AnswerFile(ShardedFile {
                            tx_id: msg.tx_id,
                            shard_config: my_shard_config.into(),
                        }),
                        request_id: RequestId::Router(Instant::now()),
                    });
                }
            }
        }

        MessageAcceptance::Ignore
    }

    async fn construct_announced_ip(&self) -> Option<Multiaddr> {
        // public address configured
        if let Some(ip) = self.config.public_address {
            let mut addr = Multiaddr::empty();
            addr.push(ip.into());
            addr.push(Protocol::Tcp(self.network_globals.listen_port_tcp()));
            return Some(addr);
        }

        // public listen address
        if let Some(addr) = self.get_listen_addr() {
            return Some(addr);
        }

        // auto detect public IP address
        let ipv4_addr = public_ip::addr_v4().await?;

        let mut addr = Multiaddr::empty();
        addr.push(Protocol::Ip4(ipv4_addr));
        addr.push(Protocol::Tcp(self.network_globals.listen_port_tcp()));

        self.network_globals
            .listen_multiaddrs
            .write()
            .insert(0, addr.clone());

        info!(
            ?addr,
            "Create public ip address to broadcast file announcement"
        );

        Some(addr)
    }

    fn get_listen_addr(&self) -> Option<Multiaddr> {
        let listen_addrs = self.network_globals.listen_multiaddrs.read();

        if self.config.private_ip_enabled {
            listen_addrs.first().cloned()
        } else {
            listen_addrs
                .iter()
                .find(|&x| Self::contains_public_ip(x))
                .cloned()
        }
    }

    fn contains_public_ip(addr: &Multiaddr) -> bool {
        for c in addr.iter() {
            match c {
                Protocol::Ip4(ip4_addr) => {
                    return !ip4_addr.is_broadcast()
                        && !ip4_addr.is_documentation()
                        && !ip4_addr.is_link_local()
                        && !ip4_addr.is_loopback()
                        && !ip4_addr.is_multicast()
                        && !ip4_addr.is_private()
                        && !ip4_addr.is_unspecified()
                }
                Protocol::Ip6(ip6_addr) => {
                    return !ip6_addr.is_loopback()
                        && !ip6_addr.is_multicast()
                        && !ip6_addr.is_unspecified()
                }
                _ => {}
            }
        }

        false
    }

    pub async fn construct_announce_file_message(
        &self,
        tx_ids: Vec<TxID>,
    ) -> Option<SignedAnnounceFile> {
        if tx_ids.is_empty() {
            return None;
        }

        let peer_id = *self.network_globals.peer_id.read();

        let addr = self.construct_announced_ip().await?;

        let timestamp = timestamp_now();
        let shard_config = self.store.get_store().get_shard_config();

        let msg = TimedMessage {
            inner: AnnounceFile {
                tx_ids,
                shard_config: shard_config.into(),
                peer_id: peer_id.into(),
                at: addr.into(),
            },
            timestamp,
        };

        let mut signed = match SignedMessage::sign_message(msg, &self.local_keypair) {
            Ok(signed) => signed,
            Err(e) => {
                error!(%e, "Failed to sign AnnounceFile message");
                return None;
            }
        };

        signed.resend_timestamp = timestamp;

        Some(signed)
    }

    async fn on_find_file(&self, from: PeerId, msg: TimedMessage<FindFile>) -> MessageAcceptance {
        // verify timestamp
        if !metrics::LIBP2P_HANDLE_PUBSUB_FIND_FILE.verify_timestamp(
            from,
            msg.timestamp,
            *PUBSUB_TIMEOUT_NETWORK,
            None,
        ) {
            return MessageAcceptance::Ignore;
        }

        // verify announced shard config if specified
        if let Some(shard_config) = msg.maybe_shard_config {
            let announced_shard_config = match ShardConfig::try_from(shard_config) {
                Ok(v) => v,
                Err(_) => return MessageAcceptance::Reject,
            };

            // forward FIND_FILE to the network if shard config mismatch
            let my_shard_config = self.store.get_store().get_shard_config();
            if !my_shard_config.intersect(&announced_shard_config) {
                return MessageAcceptance::Accept;
            }
        }

        // check if we have it
        let tx_id = msg.tx_id;
        if matches!(self.store.check_tx_completed(tx_id.seq).await, Ok(true)) {
            if let Ok(Some(tx)) = self.store.get_tx_by_seq_number(tx_id.seq).await {
                if tx.id() == tx_id {
                    trace!(?tx_id, "Found file locally, responding to FindFile query");
                    self.publish_file(tx_id).await;
                    metrics::LIBP2P_HANDLE_PUBSUB_FIND_FILE_STORE.mark(1);
                    return MessageAcceptance::Ignore;
                }
            }
        }

        // try from cache
        if let Some(mut msg) = self.file_location_cache.get_one(tx_id) {
            trace!(?tx_id, "Found file in cache, responding to FindFile query");

            msg.resend_timestamp = timestamp_now();
            self.publish_announcement(msg).await;

            metrics::LIBP2P_HANDLE_PUBSUB_FIND_FILE_CACHE.mark(1);

            return MessageAcceptance::Ignore;
        }

        // propagate FindFile query to other nodes
        metrics::LIBP2P_HANDLE_PUBSUB_FIND_FILE_FORWARD.mark(1);
        MessageAcceptance::Accept
    }

    pub async fn construct_announce_chunks_message(
        &self,
        tx_id: TxID,
        index_start: u64,
        index_end: u64,
    ) -> Option<PubsubMessage> {
        let peer_id = *self.network_globals.peer_id.read();
        let addr = self.construct_announced_ip().await?;

        let msg = AnnounceChunks {
            tx_id,
            index_start,
            index_end,
            peer_id: peer_id.into(),
            at: addr.into(),
        };

        Some(PubsubMessage::AnnounceChunks(msg.into()))
    }

    async fn on_find_chunks(
        &self,
        propagation_source: PeerId,
        msg: TimedMessage<FindChunks>,
    ) -> MessageAcceptance {
        // verify timestamp
        if !metrics::LIBP2P_HANDLE_PUBSUB_FIND_CHUNKS.verify_timestamp(
            propagation_source,
            msg.timestamp,
            *PUBSUB_TIMEOUT_NETWORK,
            None,
        ) {
            return MessageAcceptance::Ignore;
        }

        // validate message
        if msg.index_start >= msg.index_end {
            debug!(?msg, "Invalid chunk index range");
            return MessageAcceptance::Reject;
        }

        // check if we have specified chunks even file not finalized yet
        // validate end index
        let tx = match self.store.get_tx_by_seq_number(msg.tx_id.seq).await {
            Ok(Some(tx)) if tx.id() == msg.tx_id => tx,
            _ => return MessageAcceptance::Accept,
        };

        // validate index range
        if let Ok(size) = usize::try_from(tx.size) {
            let num_chunks = bytes_to_chunks(size);
            if msg.index_end > num_chunks as u64 {
                debug!(?msg, "Invalid chunk end index for FindChunks message");
                return MessageAcceptance::Reject;
            }
        }

        // TODO(qhz): check if there is better way to check existence of requested chunks.
        match self
            .store
            .get_chunks_by_tx_and_index_range(
                msg.tx_id.seq,
                msg.index_start as usize,
                msg.index_end as usize,
            )
            .await
        {
            Ok(Some(_)) => (),
            _ => return MessageAcceptance::Accept,
        };

        trace!(?msg, "Found chunks to respond FindChunks message");

        match self
            .construct_announce_chunks_message(msg.tx_id, msg.index_start, msg.index_end)
            .await
        {
            Some(msg) => {
                self.publish(msg);
                MessageAcceptance::Ignore
            }
            // propagate FindFile query to other nodes
            None => MessageAcceptance::Accept,
        }
    }

    /// Verify the announced IP address and `libp2p` seen IP address to prevent DDOS attack.
    fn verify_announced_address(&self, peer_id: &PeerId, addr: &Multiaddr) -> bool {
        let mut announced_ip = None;

        for c in addr.iter() {
            match c {
                Protocol::Ip4(addr) => announced_ip = Some(IpAddr::V4(addr)),
                Protocol::Ip6(addr) => announced_ip = Some(IpAddr::V6(addr)),
                _ => {}
            }
        }

        let announced_ip = match announced_ip {
            Some(v) => v,
            None => return false,
        };

        metrics::LIBP2P_VERIFY_ANNOUNCED_IP.mark(1);

        let seen_ips: Vec<IpAddr> = match self.network_globals.peers.read().peer_info(peer_id) {
            Some(v) => v.seen_ip_addresses().collect(),
            None => {
                // ignore file announcement from un-seen peers
                trace!(%announced_ip, "Failed to verify announced IP address, no peer info found");
                metrics::LIBP2P_VERIFY_ANNOUNCED_IP_UNSEEN.mark(1);
                return false;
            }
        };

        if seen_ips.iter().any(|x| *x == announced_ip) {
            true
        } else {
            // ignore file announcement if announced IP and seen IP mismatch
            trace!(%announced_ip, ?seen_ips, "Failed to verify announced IP address, mismatch with seen ips");
            metrics::LIBP2P_VERIFY_ANNOUNCED_IP_MISMATCH.mark(1);
            false
        }
    }

    fn on_announce_file(
        &self,
        propagation_source: PeerId,
        msg: SignedAnnounceFile,
    ) -> MessageAcceptance {
        metrics::LIBP2P_HANDLE_PUBSUB_ANNOUNCE_FILE_ANNOUNCEMENTS.mark(1);
        metrics::LIBP2P_HANDLE_PUBSUB_ANNOUNCE_FILE_FILES.mark(msg.tx_ids.len());

        // verify message signature
        if !verify_signature(&msg, &msg.peer_id, propagation_source) {
            return MessageAcceptance::Reject;
        }

        // verify public ip address if required
        let addr = msg.at.clone().into();
        if !self.config.private_ip_enabled && !Self::contains_public_ip(&addr) {
            return MessageAcceptance::Reject;
        }

        // verify announced ip address if required
        if !self.config.private_ip_enabled
            && self.config.check_announced_ip
            && !self.verify_announced_address(&msg.peer_id, &addr)
        {
            return MessageAcceptance::Reject;
        }

        // verify announced shard config
        let announced_shard_config = match ShardConfig::try_from(msg.shard_config) {
            Ok(v) => v,
            Err(_) => return MessageAcceptance::Reject,
        };

        // propagate gossip to peers
        let d = duration_since(
            msg.resend_timestamp,
            metrics::LIBP2P_HANDLE_PUBSUB_ANNOUNCE_FILE_LATENCY.clone(),
        );
        if d < TOLERABLE_DRIFT.neg() || d > *PUBSUB_TIMEOUT_NETWORK {
            debug!(?d, %propagation_source, "Invalid resend timestamp, ignoring AnnounceFile message");
            metrics::LIBP2P_HANDLE_PUBSUB_ANNOUNCE_FILE_TIMEOUT.mark(1);
            return MessageAcceptance::Ignore;
        }

        // notify sync layer if shard config matches
        let my_shard_config = self.store.get_store().get_shard_config();
        if my_shard_config.intersect(&announced_shard_config) {
            for tx_id in msg.tx_ids.iter() {
                self.send_to_sync(SyncMessage::AnnounceFileGossip {
                    tx_id: *tx_id,
                    peer_id: msg.peer_id.clone().into(),
                    addr: addr.clone(),
                });
            }
        }

        // insert message to cache
        self.file_location_cache.insert(msg);

        MessageAcceptance::Accept
    }

    fn on_announce_shard_config(
        &self,
        propagation_source: PeerId,
        source: PeerId,
        msg: TimedMessage<shared_types::ShardConfig>,
    ) -> MessageAcceptance {
        // validate timestamp
        if !metrics::LIBP2P_HANDLE_PUBSUB_ANNOUNCE_SHARD.verify_timestamp(
            propagation_source,
            msg.timestamp,
            *PUBSUB_TIMEOUT_NETWORK,
            None,
        ) {
            return MessageAcceptance::Ignore;
        }

        let shard_config = match ShardConfig::try_from(msg.inner) {
            Ok(v) => v,
            Err(_) => return MessageAcceptance::Reject,
        };

        // insert message to cache
        self.file_location_cache
            .insert_peer_config(source, shard_config);

        // notify sync layer
        self.send_to_sync(SyncMessage::AnnounceShardConfig {
            shard_config,
            peer_id: source,
        });

        MessageAcceptance::Accept
    }

    fn on_announce_chunks(
        &self,
        propagation_source: PeerId,
        msg: TimedMessage<AnnounceChunks>,
    ) -> MessageAcceptance {
        // verify timestamp
        if !metrics::LIBP2P_HANDLE_PUBSUB_ANNOUNCE_CHUNKS.verify_timestamp(
            propagation_source,
            msg.timestamp,
            *PUBSUB_TIMEOUT_NETWORK,
            None,
        ) {
            return MessageAcceptance::Ignore;
        }

        // verify public ip address if required
        let addr = msg.at.clone().into();
        if !self.config.private_ip_enabled && !Self::contains_public_ip(&addr) {
            return MessageAcceptance::Reject;
        }

        // verify announced ip address if required
        if !self.config.private_ip_enabled
            && self.config.check_announced_ip
            && !self.verify_announced_address(&msg.peer_id, &addr)
        {
            return MessageAcceptance::Reject;
        }

        // notify sync layer
        self.send_to_sync(SyncMessage::AnnounceChunksGossip { msg: msg.inner });

        MessageAcceptance::Accept
    }

    fn verify_status_message(
        &self,
        peer_id: PeerId,
        status: StatusMessage,
        network_id: NetworkIdentity,
        shard_config: &ShardConfig,
    ) -> bool {
        if status.data != network_id {
            warn!(%peer_id, ?network_id, ?status.data, "Report peer with incompatible network id");
            self.send_to_network(NetworkMessage::ReportPeer {
                peer_id,
                action: PeerAction::Fatal,
                source: ReportSource::RPC,
                msg: "Incompatible network id in StatusMessage",
            });
            return false;
        }

        let peer_shard_config = match ShardConfig::new(status.shard_id, status.num_shard) {
            Ok(v) => v,
            Err(err) => {
                warn!(%peer_id, ?status, ?err, "Report peer with invalid shard config");
                self.send_to_network(NetworkMessage::ReportPeer {
                    peer_id,
                    action: PeerAction::Fatal,
                    source: ReportSource::RPC,
                    msg: "Invalid shard config in StatusMessage",
                });
                return false;
            }
        };

        self.file_location_cache
            .insert_peer_config(peer_id, peer_shard_config);

        if !peer_shard_config.intersect(shard_config) {
            info!(%peer_id, ?shard_config, ?status, "Report peer with mismatched shard config");
            self.send_to_network(NetworkMessage::ReportPeer {
                peer_id,
                action: PeerAction::LowToleranceError,
                source: ReportSource::RPC,
                msg: "Shard config mismatch in StatusMessage",
            });
            self.send_to_network(NetworkMessage::DisconnectPeer { peer_id });
            return false;
        }

        true
    }

    async fn publish_file(&self, tx_id: TxID) {
        if let Some(batch) = self.file_batcher.write().await.add(tx_id) {
            if let Some(announcement) = self.construct_announce_file_message(batch).await {
                self.publish_announcement(announcement).await;
            }
        }
    }

    async fn publish_announcement(&self, announcement: SignedAnnounceFile) {
        if let Some(batch) = self.announcement_batcher.write().await.add(announcement) {
            self.publish(PubsubMessage::AnnounceFile(batch));
        }
    }

    /// Publish expired file announcements.
    pub async fn expire_batcher(&self) {
        if let Some(batch) = self.file_batcher.write().await.expire() {
            if let Some(announcement) = self.construct_announce_file_message(batch).await {
                self.publish_announcement(announcement).await;
            }
        }

        if let Some(batch) = self.announcement_batcher.write().await.expire() {
            self.publish(PubsubMessage::AnnounceFile(batch));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use channel::Message::*;
    use file_location_cache::{test_util::AnnounceFileBuilder, FileLocationCache};
    use network::{
        discovery::{CombinedKey, ConnectionId},
        discv5::enr::EnrBuilder,
        new_network_channel,
        rpc::{GetChunksRequest, StatusMessage, SubstreamId},
        types::FindFile,
        CombinedKeyExt, Keypair, MessageAcceptance, MessageId, Multiaddr, NetworkGlobals,
        NetworkMessage, NetworkReceiver, PeerId, PubsubMessage, Request, RequestId, Response,
        SyncId,
    };
    use shared_types::{timestamp_now, ChunkArray, ChunkArrayWithProof, FlowRangeProof, TxID};
    use storage::{
        log_store::{log_manager::LogConfig, Store},
        LogManager,
    };
    use sync::{test_util::create_2_store, SyncMessage, SyncReceiver, SyncSender};
    use task_executor::test_utils::TestRuntime;
    use tokio::sync::{
        mpsc::{self, error::TryRecvError},
        RwLock,
    };

    use crate::{peer_manager::PeerManager, Config};

    use super::*;

    struct Context {
        runtime: TestRuntime,
        network_globals: Arc<NetworkGlobals>,
        keypair: Keypair,
        network_send: NetworkSender,
        network_recv: NetworkReceiver,
        sync_send: SyncSender,
        sync_recv: SyncReceiver,
        chunk_pool_send: mpsc::UnboundedSender<ChunkPoolMessage>,
        // chunk_pool_recv: mpsc::UnboundedReceiver<ChunkPoolMessage>,
        store: Arc<dyn Store>,
        file_location_cache: Arc<FileLocationCache>,
        peers: Arc<RwLock<PeerManager>>,
    }

    impl Default for Context {
        fn default() -> Self {
            let runtime = TestRuntime::default();
            let (network_globals, keypair) = Context::new_network_globals();
            let (network_send, network_recv) = new_network_channel();
            let (sync_send, sync_recv) = channel::Channel::unbounded("test");
            let (chunk_pool_send, _chunk_pool_recv) = mpsc::unbounded_channel();

            let store = LogManager::memorydb(LogConfig::default()).unwrap();
            Self {
                runtime,
                network_globals: Arc::new(network_globals),
                keypair,
                network_send,
                network_recv,
                sync_send,
                sync_recv,
                chunk_pool_send,
                // chunk_pool_recv,
                store: Arc::new(store),
                file_location_cache: Arc::new(FileLocationCache::default()),
                peers: Arc::new(RwLock::new(PeerManager::new(Config::default()))),
            }
        }
    }

    impl Context {
        fn new_handler(&self) -> Libp2pEventHandler {
            Libp2pEventHandler::new(
                Config::default().with_private_ip_enabled(true),
                self.network_globals.clone(),
                self.network_send.clone(),
                self.sync_send.clone(),
                self.chunk_pool_send.clone(),
                self.keypair.clone(),
                storage_async::Store::new(self.store.clone(), self.runtime.task_executor.clone()),
                self.file_location_cache.clone(),
                self.peers.clone(),
            )
        }

        fn new_network_globals() -> (NetworkGlobals, Keypair) {
            let keypair = Keypair::generate_secp256k1();
            let enr_key = CombinedKey::from_libp2p(&keypair).unwrap();
            let enr = EnrBuilder::new("v4").build(&enr_key).unwrap();
            let network_globals = NetworkGlobals::new(
                enr,
                30000,
                30000,
                vec![],
                Default::default(),
                Default::default(),
            );

            let listen_addr: Multiaddr = "/ip4/127.0.0.1/tcp/30000".parse().unwrap();
            network_globals.listen_multiaddrs.write().push(listen_addr);

            (network_globals, keypair)
        }

        fn assert_status_request(&mut self, expected_peer_id: PeerId) {
            match self.network_recv.try_recv() {
                Ok(NetworkMessage::SendRequest {
                    peer_id,
                    request,
                    request_id,
                }) => {
                    assert_eq!(peer_id, expected_peer_id);
                    assert!(matches!(request, Request::Status(..)));
                    assert!(matches!(request_id, RequestId::Router(..)))
                }
                Ok(_) => panic!("Unexpected network message type received"),
                Err(e) => panic!("No network message received: {:?}", e),
            }
        }

        fn assert_file_announcement_published(&mut self, expected_tx_id: TxID) {
            match self.network_recv.try_recv() {
                Ok(NetworkMessage::Publish { messages }) => {
                    assert_eq!(messages.len(), 1);
                    assert!(
                        matches!(&messages[0], PubsubMessage::AnnounceFile(files) if files[0].tx_ids[0] == expected_tx_id)
                    );
                }
                Ok(_) => panic!("Unexpected network message type received"),
                Err(e) => panic!("No network message received: {:?}", e),
            }
        }
    }

    #[test]
    fn test_send_status() {
        let mut ctx = Context::default();
        let handler = ctx.new_handler();

        assert!(matches!(
            ctx.network_recv.try_recv(),
            Err(TryRecvError::Empty)
        ));

        let alice = PeerId::random();
        handler.send_status(alice);

        ctx.assert_status_request(alice);
    }

    #[tokio::test]
    async fn test_on_peer_connected_incoming() {
        let mut ctx = Context::default();
        let handler = ctx.new_handler();

        assert_eq!(handler.peers.read().await.size(), 0);

        let alice = PeerId::random();
        handler.on_peer_connected(alice, false).await;

        assert_eq!(handler.peers.read().await.size(), 1);
        assert!(matches!(
            ctx.network_recv.try_recv(),
            Err(TryRecvError::Empty)
        ));
        assert!(matches!(ctx.sync_recv.try_recv(), Err(TryRecvError::Empty)));
    }

    #[tokio::test]
    async fn test_on_peer_connected_outgoing() {
        let mut ctx = Context::default();
        let handler = ctx.new_handler();

        assert_eq!(handler.peers.read().await.size(), 0);

        let alice = PeerId::random();
        handler.on_peer_connected(alice, true).await;

        assert_eq!(handler.peers.read().await.size(), 1);
        ctx.assert_status_request(alice);
        assert!(matches!(ctx.sync_recv.try_recv(), Err(TryRecvError::Empty)));
    }

    #[tokio::test]
    async fn test_on_peer_disconnected() {
        let mut ctx = Context::default();
        let handler = ctx.new_handler();

        let alice = PeerId::random();
        handler.on_peer_connected(alice, false).await;
        assert_eq!(handler.peers.read().await.size(), 1);

        handler.on_peer_disconnected(alice).await;
        assert_eq!(handler.peers.read().await.size(), 0);
        assert!(matches!(
            ctx.sync_recv.try_recv(),
            Ok(Notification(SyncMessage::PeerDisconnected {peer_id})) if peer_id == alice
        ));
    }

    #[tokio::test]
    async fn test_on_rpc_request_status() {
        let mut ctx = Context::default();
        let handler = ctx.new_handler();

        let alice = PeerId::random();
        let req_id = (ConnectionId::new(4), SubstreamId(12));
        let request = Request::Status(StatusMessage {
            data: Default::default(),
            num_shard: 1,
            shard_id: 0,
        });
        handler.on_rpc_request(alice, req_id, request).await;

        match ctx.network_recv.try_recv() {
            Ok(NetworkMessage::SendResponse {
                peer_id,
                response,
                id,
            }) => {
                assert_eq!(peer_id, alice);
                assert!(matches!(response, Response::Status(..)));
                assert_eq!(id, req_id);
            }
            Ok(_) => panic!("Unexpected network message type received"),
            Err(e) => panic!("No network message received: {:?}", e),
        }
    }

    #[tokio::test]
    async fn test_on_rpc_request_get_chunks() {
        let mut ctx = Context::default();
        let handler = ctx.new_handler();

        let alice = PeerId::random();
        let id = (ConnectionId::new(4), SubstreamId(12));
        let raw_request = GetChunksRequest {
            tx_id: TxID::random_hash(7),
            index_start: 66,
            index_end: 99,
            merkle_tx_seq: 7,
        };
        handler
            .on_rpc_request(alice, id, Request::GetChunks(raw_request.clone()))
            .await;

        match ctx.sync_recv.try_recv() {
            Ok(Notification(SyncMessage::RequestChunks {
                peer_id,
                request_id,
                request,
            })) => {
                assert_eq!(peer_id, alice);
                assert_eq!(request_id, id);
                assert_eq!(request, raw_request);
            }
            Ok(_) => panic!("Unexpected sync message type received"),
            Err(e) => panic!("No sync message received: {:?}", e),
        }
    }

    #[tokio::test]
    async fn test_on_rpc_response() {
        let mut ctx = Context::default();
        let handler = ctx.new_handler();

        let alice = PeerId::random();
        let id = TxID::random_hash(555);
        let data = ChunkArrayWithProof {
            chunks: ChunkArray {
                data: vec![1, 2, 3, 4],
                start_index: 16,
            },
            proof: FlowRangeProof::new_empty(),
        };
        handler
            .on_rpc_response(
                alice,
                RequestId::Sync(Instant::now(), SyncId::SerialSync { tx_id: id }),
                Response::Chunks(data.clone()),
            )
            .await;

        match ctx.sync_recv.try_recv() {
            Ok(Notification(SyncMessage::ChunksResponse {
                peer_id,
                request_id,
                response,
            })) => {
                assert_eq!(peer_id, alice);
                assert!(matches!(request_id, SyncId::SerialSync { tx_id } if tx_id == id ));
                assert_eq!(response, data);
            }
            Ok(_) => panic!("Unexpected sync message type received"),
            Err(e) => panic!("No sync message received: {:?}", e),
        }
    }

    #[tokio::test]
    async fn test_on_rpc_error() {
        let mut ctx = Context::default();
        let handler = ctx.new_handler();

        let alice = PeerId::random();
        let id = TxID::random_hash(555);
        handler
            .on_rpc_error(
                alice,
                RequestId::Sync(Instant::now(), SyncId::SerialSync { tx_id: id }),
            )
            .await;

        match ctx.sync_recv.try_recv() {
            Ok(Notification(SyncMessage::RpcError {
                peer_id,
                request_id,
            })) => {
                assert_eq!(peer_id, alice);
                assert!(matches!(request_id, SyncId::SerialSync { tx_id } if tx_id == id ));
            }
            Ok(_) => panic!("Unexpected sync message type received"),
            Err(e) => panic!("No sync message received: {:?}", e),
        }
    }

    async fn handle_find_file_msg(
        handler: &Libp2pEventHandler,
        tx_id: TxID,
        timestamp: u32,
    ) -> MessageAcceptance {
        let (alice, bob) = (PeerId::random(), PeerId::random());
        let id = MessageId::new(b"dummy message");
        let message = PubsubMessage::FindFile(TimedMessage {
            inner: FindFile {
                tx_id,
                maybe_shard_config: None,
            },
            timestamp,
        });
        handler.on_pubsub_message(alice, bob, &id, message).await
    }

    #[tokio::test]
    async fn test_on_pubsub_find_file_invalid_timestamp() {
        let ctx = Context::default();
        let handler = ctx.new_handler();

        // message too future
        let result = handle_find_file_msg(
            &handler,
            TxID::random_hash(412),
            timestamp_now() + 10 + TOLERABLE_DRIFT.num_seconds() as u32,
        )
        .await;
        assert!(matches!(result, MessageAcceptance::Ignore));

        // message too old
        let result = handle_find_file_msg(
            &handler,
            TxID::random_hash(412),
            timestamp_now() - 10 - PUBSUB_TIMEOUT_NETWORK.num_seconds() as u32,
        )
        .await;
        assert!(matches!(result, MessageAcceptance::Ignore));
    }

    #[tokio::test]
    async fn test_on_pubsub_find_file_not_found() {
        let ctx = Context::default();
        let handler = ctx.new_handler();

        let result = handle_find_file_msg(&handler, TxID::random_hash(412), timestamp_now()).await;
        assert!(matches!(result, MessageAcceptance::Accept));
    }

    #[tokio::test]
    async fn test_on_pubsub_find_file_in_store() {
        let mut ctx = Context::default();

        // prepare store with txs
        let (_, store, txs, _) = create_2_store(vec![1314]);
        ctx.store = store;

        let handler = ctx.new_handler();

        // receive find file request
        let result = handle_find_file_msg(&handler, txs[0].id(), timestamp_now()).await;
        assert!(matches!(result, MessageAcceptance::Ignore));
        ctx.assert_file_announcement_published(txs[0].id());
    }

    #[tokio::test]
    async fn test_on_pubsub_find_file_in_cache() {
        let mut ctx = Context::default();
        let handler = ctx.new_handler();

        // prepare tx in cache
        let tx_id = TxID::random_hash(412);
        let signed = AnnounceFileBuilder::default()
            .with_tx_id(tx_id)
            .with_timestamp(timestamp_now() - 5)
            .build();
        ctx.file_location_cache.insert(signed);

        // receive find file request
        let result = handle_find_file_msg(&handler, tx_id, timestamp_now()).await;
        assert!(matches!(result, MessageAcceptance::Ignore));
        ctx.assert_file_announcement_published(tx_id);
    }

    #[tokio::test]
    async fn test_on_pubsub_announce_file_invalid_sig() {
        let ctx = Context::default();
        let handler = ctx.new_handler();

        let (alice, bob) = (PeerId::random(), PeerId::random());
        let id = MessageId::new(b"dummy message");
        let tx_id = TxID::random_hash(412);

        // change signed message
        let mut file = handler
            .construct_announce_file_message(vec![tx_id])
            .await
            .unwrap();
        let malicious_addr: Multiaddr = "/ip4/127.0.0.38/tcp/30000".parse().unwrap();
        file.inner.inner.at = malicious_addr.into();
        let message = PubsubMessage::AnnounceFile(vec![file]);

        // failed to verify signature
        let result = handler.on_pubsub_message(alice, bob, &id, message).await;
        assert!(matches!(result, MessageAcceptance::Reject));
    }

    #[tokio::test]
    async fn test_on_pubsub_announce_file() {
        let mut ctx = Context::default();
        let handler = ctx.new_handler();

        // prepare message
        let (alice, bob) = (PeerId::random(), PeerId::random());
        let id = MessageId::new(b"dummy message");
        let tx = TxID::random_hash(412);
        let message = handler
            .construct_announce_file_message(vec![tx])
            .await
            .unwrap();
        let message = PubsubMessage::AnnounceFile(vec![message]);

        // succeeded to handle
        let result = handler.on_pubsub_message(alice, bob, &id, message).await;
        assert!(matches!(result, MessageAcceptance::Accept));

        // ensure notify to sync layer
        match ctx.sync_recv.try_recv() {
            Ok(Notification(SyncMessage::AnnounceFileGossip {
                tx_id,
                peer_id,
                addr,
            })) => {
                assert_eq!(tx_id, tx);
                assert_eq!(peer_id, *ctx.network_globals.peer_id.read());
                assert_eq!(
                    addr,
                    *ctx.network_globals
                        .listen_multiaddrs
                        .read()
                        .first()
                        .unwrap()
                );
            }
            Ok(_) => panic!("Unexpected sync message type received"),
            Err(e) => panic!("No sync message received: {:?}", e),
        }

        // ensure cache updated
        assert_eq!(ctx.file_location_cache.get_all(tx).len(), 1);
    }
}
