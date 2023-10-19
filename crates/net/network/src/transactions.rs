//! Transactions management for the p2p network.

use crate::{
    cache::LruCache,
    manager::NetworkEvent,
    message::{PeerRequest, PeerRequestSender},
    metrics::{TransactionsManagerMetrics, NETWORK_POOL_TRANSACTIONS_SCOPE},
    NetworkHandle,
};
use futures::{stream::FuturesUnordered, Future, FutureExt, StreamExt};
use reth_eth_wire::{
    EthVersion, GetPooledTransactions, NewPooledTransactionHashes, NewPooledTransactionHashes66,
    NewPooledTransactionHashes68, PooledTransactions, Transactions,
};
use reth_interfaces::{
    p2p::error::{RequestError, RequestResult},
    sync::SyncStateProvider,
};
use reth_metrics::common::mpsc::UnboundedMeteredReceiver;
use reth_network_api::{Peers, ReputationChangeKind};
use reth_primitives::{
    FromRecoveredPooledTransaction, IntoRecoveredTransaction, PeerId, PooledTransactionsElement,
    TransactionSigned, TxHash, B256,
};
use reth_transaction_pool::{
    error::PoolResult, GetPooledTransactionLimit, PoolTransaction, PropagateKind,
    PropagatedTransactions, TransactionPool, ValidPoolTransaction,
};
use std::{
    collections::{hash_map::Entry, HashMap, HashSet},
    num::NonZeroUsize,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::sync::{mpsc, oneshot, oneshot::error::RecvError};
use tokio_stream::wrappers::{ReceiverStream, UnboundedReceiverStream};
use tracing::{debug, trace};

/// Cache limit of transactions to keep track of for a single peer.
const PEER_TRANSACTION_CACHE_LIMIT: usize = 1024 * 10;

/// Soft limit for NewPooledTransactions
const NEW_POOLED_TRANSACTION_HASHES_SOFT_LIMIT: usize = 4096;

/// The target size for the message of full transactions.
const MAX_FULL_TRANSACTIONS_PACKET_SIZE: usize = 100 * 1024;

/// Recommended soft limit for the number of hashes in a GetPooledTransactions message (8kb)
///
/// <https://github.com/ethereum/devp2p/blob/master/caps/eth.md#newpooledtransactionhashes-0x08>
const GET_POOLED_TRANSACTION_SOFT_LIMIT_NUM_HASHES: usize = 256;

/// Softlimit for the response size of a GetPooledTransactions message (2MB)
const GET_POOLED_TRANSACTION_SOFT_LIMIT_SIZE: GetPooledTransactionLimit =
    GetPooledTransactionLimit::SizeSoftLimit(2 * 1024 * 1024);

/// The future for inserting a function into the pool
pub type PoolImportFuture = Pin<Box<dyn Future<Output = PoolResult<TxHash>> + Send + 'static>>;

/// Api to interact with [`TransactionsManager`] task.
#[derive(Debug, Clone)]
pub struct TransactionsHandle {
    /// Command channel to the [`TransactionsManager`]
    manager_tx: mpsc::UnboundedSender<TransactionsCommand>,
}

/// Implementation of the `TransactionsHandle` API.
impl TransactionsHandle {
    fn send(&self, cmd: TransactionsCommand) {
        let _ = self.manager_tx.send(cmd);
    }

    /// Manually propagate the transaction that belongs to the hash.
    pub fn propagate(&self, hash: TxHash) {
        self.send(TransactionsCommand::PropagateHash(hash))
    }

    /// Manually propagate the transaction hash to a specific peer.
    ///
    /// Note: this only propagates if the pool contains the transaction.
    pub fn propagate_hash_to(&self, hash: TxHash, peer: PeerId) {
        self.propagate_hashes_to(Some(hash), peer)
    }

    /// Manually propagate the transaction hashes to a specific peer.
    ///
    /// Note: this only propagates the transactions that are known to the pool.
    pub fn propagate_hashes_to(&self, hash: impl IntoIterator<Item = TxHash>, peer: PeerId) {
        self.send(TransactionsCommand::PropagateHashesTo(hash.into_iter().collect(), peer))
    }

    /// Request the active peer IDs from the [`TransactionsManager`].
    pub async fn get_active_peers(&self) -> Result<HashSet<PeerId>, RecvError> {
        let (tx, rx) = oneshot::channel();
        self.send(TransactionsCommand::GetActivePeers(tx));
        rx.await
    }

    /// Manually propagate full transactions to a specific peer.
    pub fn propagate_transactions_to(&self, transactions: Vec<TxHash>, peer: PeerId) {
        self.send(TransactionsCommand::PropagateTransactionsTo(transactions, peer))
    }

    /// Request the transaction hashes known by specific peers.
    pub async fn get_transaction_hashes(
        &self,
        peers: Vec<PeerId>,
    ) -> Result<HashMap<PeerId, HashSet<TxHash>>, RecvError> {
        let (tx, rx) = oneshot::channel();
        self.send(TransactionsCommand::GetTransactionHashes { peers, tx });
        rx.await
    }

    /// Request the transaction hashes known by a specific peer.
    pub async fn get_peer_transaction_hashes(
        &self,
        peer: PeerId,
    ) -> Result<HashSet<TxHash>, RecvError> {
        let res = self.get_transaction_hashes(vec![peer]).await?;
        Ok(res.into_values().next().unwrap_or_default())
    }
}

/// Manages transactions on top of the p2p network.
///
/// This can be spawned to another task and is supposed to be run as background service while
/// [`TransactionsHandle`] is used as frontend to send commands to.
///
/// The [`TransactionsManager`] is responsible for:
///    - handling incoming eth messages for transactions.
///    - serving transaction requests.
///    - propagate transactions
///
/// This type communicates with the [`NetworkManager`](crate::NetworkManager) in both directions.
///   - receives incoming network messages.
///   - sends messages to dispatch (responses, propagate tx)
///
/// It is directly connected to the [`TransactionPool`] to retrieve requested transactions and
/// propagate new transactions over the network.
#[derive(Debug)]
#[must_use = "Manager does nothing unless polled."]
pub struct TransactionsManager<Pool> {
    /// Access to the transaction pool.
    pool: Pool,
    /// Network access.
    network: NetworkHandle,
    /// Subscriptions to all network related events.
    ///
    /// From which we get all new incoming transaction related messages.
    network_events: UnboundedReceiverStream<NetworkEvent>,
    /// All currently active requests for pooled transactions.
    inflight_requests: FuturesUnordered<GetPooledTxRequestFut>,
    /// All currently pending transactions grouped by peers.
    ///
    /// This way we can track incoming transactions and prevent multiple pool imports for the same
    /// transaction
    transactions_by_peers: HashMap<TxHash, Vec<PeerId>>,
    /// Transactions that are currently imported into the `Pool`
    pool_imports: FuturesUnordered<PoolImportFuture>,
    /// All the connected peers.
    peers: HashMap<PeerId, Peer>,
    /// Send half for the command channel.
    command_tx: mpsc::UnboundedSender<TransactionsCommand>,
    /// Incoming commands from [`TransactionsHandle`].
    command_rx: UnboundedReceiverStream<TransactionsCommand>,
    /// Incoming commands from [`TransactionsHandle`].
    pending_transactions: ReceiverStream<TxHash>,
    /// Incoming events from the [`NetworkManager`](crate::NetworkManager).
    transaction_events: UnboundedMeteredReceiver<NetworkTransactionEvent>,
    /// TransactionsManager metrics
    metrics: TransactionsManagerMetrics,
}

impl<Pool: TransactionPool> TransactionsManager<Pool> {
    /// Sets up a new instance.
    ///
    /// Note: This expects an existing [`NetworkManager`](crate::NetworkManager) instance.
    pub fn new(
        network: NetworkHandle,
        pool: Pool,
        from_network: mpsc::UnboundedReceiver<NetworkTransactionEvent>,
    ) -> Self {
        let network_events = network.event_listener();
        let (command_tx, command_rx) = mpsc::unbounded_channel();

        // install a listener for new pending transactions that are allowed to be propagated over
        // the network
        let pending = pool.pending_transactions_listener();

        Self {
            pool,
            network,
            network_events,
            inflight_requests: Default::default(),
            transactions_by_peers: Default::default(),
            pool_imports: Default::default(),
            peers: Default::default(),
            command_tx,
            command_rx: UnboundedReceiverStream::new(command_rx),
            pending_transactions: ReceiverStream::new(pending),
            transaction_events: UnboundedMeteredReceiver::new(
                from_network,
                NETWORK_POOL_TRANSACTIONS_SCOPE,
            ),
            metrics: Default::default(),
        }
    }
}

// === impl TransactionsManager ===

impl<Pool> TransactionsManager<Pool>
where
    Pool: TransactionPool,
{
    /// Returns a new handle that can send commands to this type.
    pub fn handle(&self) -> TransactionsHandle {
        TransactionsHandle { manager_tx: self.command_tx.clone() }
    }
}

impl<Pool> TransactionsManager<Pool>
where
    Pool: TransactionPool + 'static,
{
    #[inline]
    fn update_import_metrics(&self) {
        self.metrics.pending_pool_imports.set(self.pool_imports.len() as f64);
    }

    #[inline]
    fn update_request_metrics(&self) {
        self.metrics.inflight_transaction_requests.set(self.inflight_requests.len() as f64);
    }

    /// Request handler for an incoming request for transactions
    fn on_get_pooled_transactions(
        &mut self,
        peer_id: PeerId,
        request: GetPooledTransactions,
        response: oneshot::Sender<RequestResult<PooledTransactions>>,
    ) {
        if let Some(peer) = self.peers.get_mut(&peer_id) {
            let transactions = self
                .pool
                .get_pooled_transaction_elements(request.0, GET_POOLED_TRANSACTION_SOFT_LIMIT_SIZE);

            // we sent a response at which point we assume that the peer is aware of the
            // transactions
            peer.transactions.extend(transactions.iter().map(|tx| *tx.hash()));

            let resp = PooledTransactions(transactions);
            let _ = response.send(Ok(resp));
        }
    }

    /// Invoked when a new transaction is pending in the local pool.
    ///
    /// When new transactions appear in the pool, we propagate them to the network using the
    /// `Transactions` and `NewPooledTransactionHashes` messages. The Transactions message relays
    /// complete transaction objects and is typically sent to a small, random fraction of connected
    /// peers.
    ///
    /// All other peers receive a notification of the transaction hash and can request the
    /// complete transaction object if it is unknown to them. The dissemination of complete
    /// transactions to a fraction of peers usually ensures that all nodes receive the transaction
    /// and won't need to request it.
    fn on_new_transactions(&mut self, hashes: Vec<TxHash>) {
        // Nothing to propagate while initially syncing
        if self.network.is_initially_syncing() {
            return
        }

        trace!(target: "net::tx", num_hashes=?hashes.len(), "Start propagating transactions");

        // This fetches all transaction from the pool, including the blob transactions, which are
        // only ever sent as hashes.
        let propagated = self.propagate_transactions(
            self.pool.get_all(hashes).into_iter().map(PropagateTransaction::new).collect(),
        );

        // notify pool so events get fired
        self.pool.on_propagated(propagated);
    }

    /// Propagate the transactions to all connected peers either as full objects or hashes
    ///
    /// The message for new pooled hashes depends on the negotiated version of the stream.
    /// See [NewPooledTransactionHashes]
    ///
    /// Note: EIP-4844 are disallowed from being broadcast in full and are only ever sent as hashes, see also <https://eips.ethereum.org/EIPS/eip-4844#networking>.
    fn propagate_transactions(
        &mut self,
        to_propagate: Vec<PropagateTransaction>,
    ) -> PropagatedTransactions {
        let mut propagated = PropagatedTransactions::default();

        // send full transactions to a fraction fo the connected peers (square root of the total
        // number of connected peers)
        let max_num_full = (self.peers.len() as f64).sqrt() as usize + 1;

        // Note: Assuming ~random~ order due to random state of the peers map hasher
        for (peer_idx, (peer_id, peer)) in self.peers.iter_mut().enumerate() {
            // filter all transactions unknown to the peer
            let mut hashes = PooledTransactionsHashesBuilder::new(peer.version);
            let mut full_transactions = FullTransactionsBuilder::default();

            // Iterate through the transactions to propagate and fill the hashes and full
            // transaction lists, before deciding whether or not to send full transactions to the
            // peer.
            for tx in to_propagate.iter() {
                if peer.transactions.insert(tx.hash()) {
                    hashes.push(tx);

                    // Do not send full 4844 transaction hashes to peers.
                    //
                    //  Nodes MUST NOT automatically broadcast blob transactions to their peers.
                    //  Instead, those transactions are only announced using
                    //  `NewPooledTransactionHashes` messages, and can then be manually requested
                    //  via `GetPooledTransactions`.
                    //
                    // From: <https://eips.ethereum.org/EIPS/eip-4844#networking>
                    if !tx.transaction.is_eip4844() {
                        full_transactions.push(tx);
                    }
                }
            }
            let mut new_pooled_hashes = hashes.build();

            if !new_pooled_hashes.is_empty() {
                // determine whether to send full tx objects or hashes. If there are no full
                // transactions, try to send hashes.
                if peer_idx > max_num_full || full_transactions.is_empty() {
                    // enforce tx soft limit per message for the (unlikely) event the number of
                    // hashes exceeds it
                    new_pooled_hashes.truncate(NEW_POOLED_TRANSACTION_HASHES_SOFT_LIMIT);

                    for hash in new_pooled_hashes.iter_hashes().copied() {
                        propagated.0.entry(hash).or_default().push(PropagateKind::Hash(*peer_id));
                    }

                    trace!(target: "net::tx", ?peer_id, num_txs=?new_pooled_hashes.len(), "Propagating tx hashes to peer");

                    // send hashes of transactions
                    self.network.send_transactions_hashes(*peer_id, new_pooled_hashes);
                } else {
                    let new_full_transactions = full_transactions.build();

                    for tx in new_full_transactions.iter() {
                        propagated
                            .0
                            .entry(tx.hash())
                            .or_default()
                            .push(PropagateKind::Full(*peer_id));
                    }

                    trace!(target: "net::tx", ?peer_id, num_txs=?new_full_transactions.len(), "Propagating full transactions to peer");

                    // send full transactions
                    self.network.send_transactions(*peer_id, new_full_transactions);
                }
            }
        }

        // Update propagated transactions metrics
        self.metrics.propagated_transactions.increment(propagated.0.len() as u64);

        propagated
    }

    /// Propagate the full transactions to a specific peer
    ///
    /// Returns the propagated transactions
    fn propagate_full_transactions_to_peer(
        &mut self,
        txs: Vec<TxHash>,
        peer_id: PeerId,
    ) -> Option<PropagatedTransactions> {
        trace!(target: "net::tx", ?peer_id, "Propagating transactions to peer");

        let peer = self.peers.get_mut(&peer_id)?;
        let mut propagated = PropagatedTransactions::default();

        // filter all transactions unknown to the peer
        let mut full_transactions = FullTransactionsBuilder::default();

        let to_propagate = self
            .pool
            .get_all(txs)
            .into_iter()
            .filter(|tx| !tx.transaction.is_eip4844())
            .map(PropagateTransaction::new);

        // Iterate through the transactions to propagate and fill the hashes and full transaction
        for tx in to_propagate {
            if peer.transactions.insert(tx.hash()) {
                full_transactions.push(&tx);
            }
        }

        if full_transactions.transactions.is_empty() {
            // nothing to propagate
            return None
        }

        let new_full_transactions = full_transactions.build();
        for tx in new_full_transactions.iter() {
            propagated.0.entry(tx.hash()).or_default().push(PropagateKind::Full(peer_id));
        }
        // send full transactions
        self.network.send_transactions(peer_id, new_full_transactions);

        // Update propagated transactions metrics
        self.metrics.propagated_transactions.increment(propagated.0.len() as u64);

        Some(propagated)
    }

    /// Propagate the transaction hashes to the given peer
    ///
    /// Note: This will only send the hashes for transactions that exist in the pool.
    fn propagate_hashes_to(&mut self, hashes: Vec<TxHash>, peer_id: PeerId) {
        trace!(target: "net::tx", "Start propagating transactions as hashes");

        // This fetches a transactions from the pool, including the blob transactions, which are
        // only ever sent as hashes.
        let propagated = {
            let Some(peer) = self.peers.get_mut(&peer_id) else {
                // no such peer
                return
            };

            let to_propagate: Vec<PropagateTransaction> =
                self.pool.get_all(hashes).into_iter().map(PropagateTransaction::new).collect();

            let mut propagated = PropagatedTransactions::default();

            // check if transaction is known to peer
            let mut hashes = PooledTransactionsHashesBuilder::new(peer.version);

            for tx in to_propagate {
                if peer.transactions.insert(tx.hash()) {
                    hashes.push(&tx);
                }
            }

            let new_pooled_hashes = hashes.build();

            if new_pooled_hashes.is_empty() {
                // nothing to propagate
                return
            }

            for hash in new_pooled_hashes.iter_hashes().copied() {
                propagated.0.entry(hash).or_default().push(PropagateKind::Hash(peer_id));
            }

            // send hashes of transactions
            self.network.send_transactions_hashes(peer_id, new_pooled_hashes);

            // Update propagated transactions metrics
            self.metrics.propagated_transactions.increment(propagated.0.len() as u64);

            propagated
        };

        // notify pool so events get fired
        self.pool.on_propagated(propagated);
    }

    /// Request handler for an incoming `NewPooledTransactionHashes`
    fn on_new_pooled_transaction_hashes(
        &mut self,
        peer_id: PeerId,
        msg: NewPooledTransactionHashes,
    ) {
        // If the node is initially syncing, ignore transactions
        if self.network.is_initially_syncing() {
            return
        }

        let mut num_already_seen = 0;

        if let Some(peer) = self.peers.get_mut(&peer_id) {
            let mut hashes = msg.into_hashes();
            // keep track of the transactions the peer knows
            for tx in hashes.iter().copied() {
                if !peer.transactions.insert(tx) {
                    num_already_seen += 1;
                }
            }

            self.pool.retain_unknown(&mut hashes);

            if hashes.is_empty() {
                // nothing to request
                return
            }

            // enforce recommended soft limit, however the peer may enforce an arbitrary limit on
            // the response (2MB)
            hashes.truncate(GET_POOLED_TRANSACTION_SOFT_LIMIT_NUM_HASHES);

            // request the missing transactions
            let (response, rx) = oneshot::channel();
            let req = PeerRequest::GetPooledTransactions {
                request: GetPooledTransactions(hashes),
                response,
            };

            if peer.request_tx.try_send(req).is_ok() {
                self.inflight_requests.push(GetPooledTxRequestFut::new(peer_id, rx))
            } else {
                // peer channel is saturated, drop the request
                self.metrics.egress_peer_channel_full.increment(1);
                return
            }

            if num_already_seen > 0 {
                self.metrics.messages_with_already_seen_hashes.increment(1);
                debug!(target: "net::tx", num_hashes=%num_already_seen, ?peer_id, client=?peer.client_version, "Peer sent already seen hashes");
            }
        }

        if num_already_seen > 0 {
            self.report_already_seen(peer_id);
        }
    }

    /// Handles dedicated transaction events related to the `eth` protocol.
    fn on_network_tx_event(&mut self, event: NetworkTransactionEvent) {
        match event {
            NetworkTransactionEvent::IncomingTransactions { peer_id, msg } => {
                // ensure we didn't receive any blob transactions as these are disallowed to be
                // broadcasted in full

                let has_blob_txs = msg.has_eip4844();

                let non_blob_txs = msg
                    .0
                    .into_iter()
                    .map(PooledTransactionsElement::try_from_broadcast)
                    .filter_map(Result::ok)
                    .collect();

                self.import_transactions(peer_id, non_blob_txs, TransactionSource::Broadcast);

                if has_blob_txs {
                    self.report_peer(peer_id, ReputationChangeKind::BadTransactions);
                }
            }
            NetworkTransactionEvent::IncomingPooledTransactionHashes { peer_id, msg } => {
                self.on_new_pooled_transaction_hashes(peer_id, msg)
            }
            NetworkTransactionEvent::GetPooledTransactions { peer_id, request, response } => {
                self.on_get_pooled_transactions(peer_id, request, response)
            }
        }
    }

    /// Handles a command received from a detached [`TransactionsHandle`]
    fn on_command(&mut self, cmd: TransactionsCommand) {
        match cmd {
            TransactionsCommand::PropagateHash(hash) => self.on_new_transactions(vec![hash]),
            TransactionsCommand::PropagateHashesTo(hashes, peer) => {
                self.propagate_hashes_to(hashes, peer)
            }
            TransactionsCommand::GetActivePeers(tx) => {
                let peers = self.peers.keys().copied().collect::<HashSet<_>>();
                tx.send(peers).ok();
            }
            TransactionsCommand::PropagateTransactionsTo(_txs, _peer) => {
                if let Some(propagated) = self.propagate_full_transactions_to_peer(_txs, _peer) {
                    self.pool.on_propagated(propagated);
                }
            }
            TransactionsCommand::GetTransactionHashes { peers, tx } => {
                let mut res = HashMap::with_capacity(peers.len());
                for peer_id in peers {
                    let hashes = self
                        .peers
                        .get(&peer_id)
                        .map(|peer| peer.transactions.iter().copied().collect::<HashSet<_>>())
                        .unwrap_or_default();
                    res.insert(peer_id, hashes);
                }
                tx.send(res).ok();
            }
        }
    }

    /// Handles a received event related to common network events.
    fn on_network_event(&mut self, event: NetworkEvent) {
        match event {
            NetworkEvent::SessionClosed { peer_id, .. } => {
                // remove the peer
                self.peers.remove(&peer_id);
            }
            NetworkEvent::SessionEstablished {
                peer_id, client_version, messages, version, ..
            } => {
                // insert a new peer into the peerset
                self.peers.insert(
                    peer_id,
                    Peer {
                        transactions: LruCache::new(
                            NonZeroUsize::new(PEER_TRANSACTION_CACHE_LIMIT).unwrap(),
                        ),
                        request_tx: messages,
                        version,
                        client_version,
                    },
                );

                // Send a `NewPooledTransactionHashes` to the peer with up to
                // `NEW_POOLED_TRANSACTION_HASHES_SOFT_LIMIT` transactions in the
                // pool
                if !self.network.is_initially_syncing() {
                    let peer = self.peers.get_mut(&peer_id).expect("is present; qed");

                    let mut msg_builder = PooledTransactionsHashesBuilder::new(version);

                    let pooled_txs =
                        self.pool.pooled_transactions_max(NEW_POOLED_TRANSACTION_HASHES_SOFT_LIMIT);
                    if pooled_txs.is_empty() {
                        // do not send a message if there are no transactions in the pool
                        return
                    }

                    for pooled_tx in pooled_txs.into_iter() {
                        peer.transactions.insert(*pooled_tx.hash());
                        msg_builder.push_pooled(pooled_tx);
                    }

                    let msg = msg_builder.build();
                    self.network.send_transactions_hashes(peer_id, msg);
                }
            }
            _ => {}
        }
    }

    /// Starts the import process for the given transactions.
    fn import_transactions(
        &mut self,
        peer_id: PeerId,
        transactions: Vec<PooledTransactionsElement>,
        source: TransactionSource,
    ) {
        // If the node is pipeline syncing, ignore transactions
        if self.network.is_initially_syncing() {
            return
        }

        // tracks the quality of the given transactions
        let mut has_bad_transactions = false;
        let mut num_already_seen = 0;

        if let Some(peer) = self.peers.get_mut(&peer_id) {
            for tx in transactions {
                // recover transaction
                let tx = if let Ok(tx) = tx.try_into_ecrecovered() {
                    tx
                } else {
                    has_bad_transactions = true;
                    continue
                };

                // track that the peer knows this transaction, but only if this is a new broadcast.
                // If we received the transactions as the response to our GetPooledTransactions
                // requests (based on received `NewPooledTransactionHashes`) then we already
                // recorded the hashes in [`Self::on_new_pooled_transaction_hashes`]
                if source.is_broadcast() && !peer.transactions.insert(*tx.hash()) {
                    num_already_seen += 1;
                }

                match self.transactions_by_peers.entry(*tx.hash()) {
                    Entry::Occupied(mut entry) => {
                        // transaction was already inserted
                        entry.get_mut().push(peer_id);
                    }
                    Entry::Vacant(entry) => {
                        // this is a new transaction that should be imported into the pool
                        let pool_transaction = <Pool::Transaction as FromRecoveredPooledTransaction>::from_recovered_transaction(tx);

                        let pool = self.pool.clone();

                        let import = Box::pin(async move {
                            pool.add_external_transaction(pool_transaction).await
                        });

                        self.pool_imports.push(import);
                        entry.insert(vec![peer_id]);
                    }
                }
            }

            if num_already_seen > 0 {
                self.metrics.messages_with_already_seen_transactions.increment(1);
                debug!(target: "net::tx", num_txs=%num_already_seen, ?peer_id, client=?peer.client_version, "Peer sent already seen transactions");
            }
        }

        if has_bad_transactions || num_already_seen > 0 {
            self.report_already_seen(peer_id);
        }
    }

    fn report_peer(&self, peer_id: PeerId, kind: ReputationChangeKind) {
        trace!(target: "net::tx", ?peer_id, ?kind);
        self.network.reputation_change(peer_id, kind);
        self.metrics.reported_bad_transactions.increment(1);
    }

    fn on_request_error(&self, peer_id: PeerId, req_err: RequestError) {
        let kind = match req_err {
            RequestError::UnsupportedCapability => ReputationChangeKind::BadProtocol,
            RequestError::Timeout => ReputationChangeKind::Timeout,
            RequestError::ChannelClosed | RequestError::ConnectionDropped => {
                // peer is already disconnected
                return
            }
            RequestError::BadResponse => ReputationChangeKind::BadTransactions,
        };
        self.report_peer(peer_id, kind);
    }

    fn report_already_seen(&self, peer_id: PeerId) {
        trace!(target: "net::tx", ?peer_id, "Penalizing peer for already seen transaction");
        self.network.reputation_change(peer_id, ReputationChangeKind::AlreadySeenTransaction);
    }

    /// Clear the transaction
    fn on_good_import(&mut self, hash: TxHash) {
        self.transactions_by_peers.remove(&hash);
    }

    /// Penalize the peers that sent the bad transaction
    fn on_bad_import(&mut self, hash: TxHash) {
        if let Some(peers) = self.transactions_by_peers.remove(&hash) {
            for peer_id in peers {
                self.report_peer(peer_id, ReputationChangeKind::BadTransactions);
            }
        }
    }
}

/// An endless future.
///
/// This should be spawned or used as part of `tokio::select!`.
impl<Pool> Future for TransactionsManager<Pool>
where
    Pool: TransactionPool + Unpin + 'static,
    <Pool as TransactionPool>::Transaction: IntoRecoveredTransaction,
{
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        // drain network/peer related events
        while let Poll::Ready(Some(event)) = this.network_events.poll_next_unpin(cx) {
            this.on_network_event(event);
        }

        // drain commands
        while let Poll::Ready(Some(cmd)) = this.command_rx.poll_next_unpin(cx) {
            this.on_command(cmd);
        }

        // drain incoming transaction events
        while let Poll::Ready(Some(event)) = this.transaction_events.poll_next_unpin(cx) {
            this.on_network_tx_event(event);
        }

        this.update_request_metrics();

        // Advance all requests.
        while let Poll::Ready(Some(GetPooledTxResponse { peer_id, result })) =
            this.inflight_requests.poll_next_unpin(cx)
        {
            match result {
                Ok(Ok(txs)) => {
                    this.import_transactions(peer_id, txs.0, TransactionSource::Response)
                }
                Ok(Err(req_err)) => {
                    this.on_request_error(peer_id, req_err);
                }
                Err(_) => {
                    // request channel closed/dropped
                    this.on_request_error(peer_id, RequestError::ChannelClosed)
                }
            }
        }

        this.update_request_metrics();
        this.update_import_metrics();

        // Advance all imports
        while let Poll::Ready(Some(import_res)) = this.pool_imports.poll_next_unpin(cx) {
            match import_res {
                Ok(hash) => {
                    this.on_good_import(hash);
                }
                Err(err) => {
                    // if we're _currently_ syncing and the transaction is bad we ignore it,
                    // otherwise we penalize the peer that sent the bad
                    // transaction with the assumption that the peer should have
                    // known that this transaction is bad. (e.g. consensus
                    // rules)
                    if err.is_bad_transaction() && !this.network.is_syncing() {
                        trace!(target: "net::tx", ?err, "Bad transaction import");
                        this.on_bad_import(*err.hash());
                        continue
                    }
                    this.on_good_import(*err.hash());
                }
            }
        }

        this.update_import_metrics();

        // handle and propagate new transactions
        let mut new_txs = Vec::new();
        while let Poll::Ready(Some(hash)) = this.pending_transactions.poll_next_unpin(cx) {
            new_txs.push(hash);
        }
        if !new_txs.is_empty() {
            this.on_new_transactions(new_txs);
        }

        // all channels are fully drained and import futures pending

        Poll::Pending
    }
}

/// A transaction that's about to be propagated to multiple peers.
struct PropagateTransaction {
    size: usize,
    transaction: Arc<TransactionSigned>,
}

// === impl PropagateTransaction ===

impl PropagateTransaction {
    fn hash(&self) -> TxHash {
        self.transaction.hash()
    }

    /// Create a new instance from a pooled transaction
    fn new<T: PoolTransaction>(tx: Arc<ValidPoolTransaction<T>>) -> Self {
        let size = tx.encoded_length();
        let transaction = Arc::new(tx.transaction.to_recovered_transaction().into_signed());
        Self { size, transaction }
    }
}

/// Helper type for constructing the full transaction message that enforces the
/// `MAX_FULL_TRANSACTIONS_PACKET_SIZE`
#[derive(Default)]
struct FullTransactionsBuilder {
    total_size: usize,
    transactions: Vec<Arc<TransactionSigned>>,
}

// === impl FullTransactionsBuilder ===

impl FullTransactionsBuilder {
    /// Append a transaction to the list if it doesn't exceed the maximum size.
    fn push(&mut self, transaction: &PropagateTransaction) {
        let new_size = self.total_size + transaction.size;
        if new_size > MAX_FULL_TRANSACTIONS_PACKET_SIZE {
            return
        }

        self.total_size = new_size;
        self.transactions.push(Arc::clone(&transaction.transaction));
    }

    /// Returns whether or not any transactions are in the [FullTransactionsBuilder].
    fn is_empty(&self) -> bool {
        self.transactions.is_empty()
    }

    /// returns the list of transactions.
    fn build(self) -> Vec<Arc<TransactionSigned>> {
        self.transactions
    }
}

/// A helper type to create the pooled transactions message based on the negotiated version of the
/// session with the peer
enum PooledTransactionsHashesBuilder {
    Eth66(NewPooledTransactionHashes66),
    Eth68(NewPooledTransactionHashes68),
}

// === impl PooledTransactionsHashesBuilder ===

impl PooledTransactionsHashesBuilder {
    /// Push a transaction from the pool to the list.
    fn push_pooled<T: PoolTransaction>(&mut self, pooled_tx: Arc<ValidPoolTransaction<T>>) {
        match self {
            PooledTransactionsHashesBuilder::Eth66(msg) => msg.0.push(*pooled_tx.hash()),
            PooledTransactionsHashesBuilder::Eth68(msg) => {
                msg.hashes.push(*pooled_tx.hash());
                msg.sizes.push(pooled_tx.encoded_length());
                msg.types.push(pooled_tx.transaction.tx_type());
            }
        }
    }

    fn push(&mut self, tx: &PropagateTransaction) {
        match self {
            PooledTransactionsHashesBuilder::Eth66(msg) => msg.0.push(tx.hash()),
            PooledTransactionsHashesBuilder::Eth68(msg) => {
                msg.hashes.push(tx.hash());
                msg.sizes.push(tx.size);
                msg.types.push(tx.transaction.tx_type().into());
            }
        }
    }

    /// Create a builder for the negotiated version of the peer's session
    fn new(version: EthVersion) -> Self {
        match version {
            EthVersion::Eth66 | EthVersion::Eth67 => {
                PooledTransactionsHashesBuilder::Eth66(Default::default())
            }
            EthVersion::Eth68 => PooledTransactionsHashesBuilder::Eth68(Default::default()),
        }
    }

    fn build(self) -> NewPooledTransactionHashes {
        match self {
            PooledTransactionsHashesBuilder::Eth66(msg) => msg.into(),
            PooledTransactionsHashesBuilder::Eth68(msg) => msg.into(),
        }
    }
}

/// How we received the transactions.
enum TransactionSource {
    /// Transactions were broadcast to us via [`Transactions`] message.
    Broadcast,
    /// Transactions were sent as the response of [`GetPooledTxRequest`] issued by us.
    Response,
}

// === impl TransactionSource ===

impl TransactionSource {
    /// Whether the transaction were sent as broadcast.
    fn is_broadcast(&self) -> bool {
        matches!(self, TransactionSource::Broadcast)
    }
}

/// An inflight request for `PooledTransactions` from a peer
struct GetPooledTxRequest {
    peer_id: PeerId,
    response: oneshot::Receiver<RequestResult<PooledTransactions>>,
}

struct GetPooledTxResponse {
    peer_id: PeerId,
    result: Result<RequestResult<PooledTransactions>, RecvError>,
}

#[must_use = "futures do nothing unless polled"]
#[pin_project::pin_project]
struct GetPooledTxRequestFut {
    #[pin]
    inner: Option<GetPooledTxRequest>,
}

impl GetPooledTxRequestFut {
    fn new(
        peer_id: PeerId,
        response: oneshot::Receiver<RequestResult<PooledTransactions>>,
    ) -> Self {
        Self { inner: Some(GetPooledTxRequest { peer_id, response }) }
    }
}

impl Future for GetPooledTxRequestFut {
    type Output = GetPooledTxResponse;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut req = self.as_mut().project().inner.take().expect("polled after completion");
        match req.response.poll_unpin(cx) {
            Poll::Ready(result) => {
                Poll::Ready(GetPooledTxResponse { peer_id: req.peer_id, result })
            }
            Poll::Pending => {
                self.project().inner.set(Some(req));
                Poll::Pending
            }
        }
    }
}

/// Tracks a single peer
#[derive(Debug)]
struct Peer {
    /// Keeps track of transactions that we know the peer has seen.
    transactions: LruCache<B256>,
    /// A communication channel directly to the peer's session task.
    request_tx: PeerRequestSender,
    /// negotiated version of the session.
    version: EthVersion,
    /// The peer's client version.
    #[allow(unused)]
    client_version: Arc<String>,
}

/// Commands to send to the [`TransactionsManager`]
#[derive(Debug)]
enum TransactionsCommand {
    /// Propagate a transaction hash to the network.
    PropagateHash(B256),
    /// Propagate transaction hashes to a specific peer.
    PropagateHashesTo(Vec<B256>, PeerId),
    /// Request the list of active peer IDs from the [`TransactionsManager`].
    GetActivePeers(oneshot::Sender<HashSet<PeerId>>),
    /// Propagate a collection of full transactions to a specific peer.
    PropagateTransactionsTo(Vec<TxHash>, PeerId),
    /// Request transaction hashes known by specific peers from the [`TransactionsManager`].
    GetTransactionHashes {
        peers: Vec<PeerId>,
        tx: oneshot::Sender<HashMap<PeerId, HashSet<TxHash>>>,
    },
}

/// All events related to transactions emitted by the network.
#[derive(Debug)]
#[allow(missing_docs)]
pub enum NetworkTransactionEvent {
    /// Received list of transactions from the given peer.
    ///
    /// This represents transactions that were broadcasted to use from the peer.
    IncomingTransactions { peer_id: PeerId, msg: Transactions },
    /// Received list of transactions hashes to the given peer.
    IncomingPooledTransactionHashes { peer_id: PeerId, msg: NewPooledTransactionHashes },
    /// Incoming `GetPooledTransactions` request from a peer.
    GetPooledTransactions {
        peer_id: PeerId,
        request: GetPooledTransactions,
        response: oneshot::Sender<RequestResult<PooledTransactions>>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{test_utils::Testnet, NetworkConfigBuilder, NetworkManager};
    use alloy_rlp::Decodable;
    use reth_interfaces::sync::{NetworkSyncUpdater, SyncState};
    use reth_network_api::NetworkInfo;
    use reth_primitives::hex;
    use reth_provider::test_utils::NoopProvider;
    use reth_transaction_pool::test_utils::{testing_pool, MockTransaction};
    use secp256k1::SecretKey;
    use std::future::poll_fn;

    #[tokio::test(flavor = "multi_thread")]
    #[cfg_attr(not(feature = "geth-tests"), ignore)]
    async fn test_ignored_tx_broadcasts_while_initially_syncing() {
        reth_tracing::init_test_tracing();
        let net = Testnet::create(3).await;

        let mut handles = net.handles();
        let handle0 = handles.next().unwrap();
        let handle1 = handles.next().unwrap();

        drop(handles);
        let handle = net.spawn();

        let listener0 = handle0.event_listener();
        handle0.add_peer(*handle1.peer_id(), handle1.local_addr());
        let secret_key = SecretKey::new(&mut rand::thread_rng());

        let client = NoopProvider::default();
        let pool = testing_pool();
        let config = NetworkConfigBuilder::new(secret_key)
            .disable_discovery()
            .listener_port(0)
            .build(client);
        let (network_handle, network, mut transactions, _) = NetworkManager::new(config)
            .await
            .unwrap()
            .into_builder()
            .transactions(pool.clone())
            .split_with_handle();

        tokio::task::spawn(network);

        // go to syncing (pipeline sync)
        network_handle.update_sync_state(SyncState::Syncing);
        assert!(NetworkInfo::is_syncing(&network_handle));
        assert!(NetworkInfo::is_initially_syncing(&network_handle));

        // wait for all initiator connections
        let mut established = listener0.take(2);
        while let Some(ev) = established.next().await {
            match ev {
                NetworkEvent::SessionEstablished {
                    peer_id,
                    remote_addr,
                    client_version,
                    capabilities,
                    messages,
                    status,
                    version,
                } => {
                    // to insert a new peer in transactions peerset
                    transactions.on_network_event(NetworkEvent::SessionEstablished {
                        peer_id,
                        remote_addr,
                        client_version,
                        capabilities,
                        messages,
                        status,
                        version,
                    })
                }
                NetworkEvent::PeerAdded(_peer_id) => continue,
                ev => {
                    panic!("unexpected event {ev:?}")
                }
            }
        }
        // random tx: <https://etherscan.io/getRawTx?tx=0x9448608d36e721ef403c53b00546068a6474d6cbab6816c3926de449898e7bce>
        let input = hex!("02f871018302a90f808504890aef60826b6c94ddf4c5025d1a5742cf12f74eec246d4432c295e487e09c3bbcc12b2b80c080a0f21a4eacd0bf8fea9c5105c543be5a1d8c796516875710fafafdf16d16d8ee23a001280915021bb446d1973501a67f93d2b38894a514b976e7b46dc2fe54598d76");
        let signed_tx = TransactionSigned::decode(&mut &input[..]).unwrap();
        transactions.on_network_tx_event(NetworkTransactionEvent::IncomingTransactions {
            peer_id: *handle1.peer_id(),
            msg: Transactions(vec![signed_tx.clone()]),
        });
        poll_fn(|cx| {
            let _ = transactions.poll_unpin(cx);
            Poll::Ready(())
        })
        .await;
        assert!(pool.is_empty());
        handle.terminate().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    #[cfg_attr(not(feature = "geth-tests"), ignore)]
    async fn test_tx_broadcasts_through_two_syncs() {
        reth_tracing::init_test_tracing();
        let net = Testnet::create(3).await;

        let mut handles = net.handles();
        let handle0 = handles.next().unwrap();
        let handle1 = handles.next().unwrap();

        drop(handles);
        let handle = net.spawn();

        let listener0 = handle0.event_listener();
        handle0.add_peer(*handle1.peer_id(), handle1.local_addr());
        let secret_key = SecretKey::new(&mut rand::thread_rng());

        let client = NoopProvider::default();
        let pool = testing_pool();
        let config = NetworkConfigBuilder::new(secret_key)
            .disable_discovery()
            .listener_port(0)
            .build(client);
        let (network_handle, network, mut transactions, _) = NetworkManager::new(config)
            .await
            .unwrap()
            .into_builder()
            .transactions(pool.clone())
            .split_with_handle();

        tokio::task::spawn(network);

        // go to syncing (pipeline sync) to idle and then to syncing (live)
        network_handle.update_sync_state(SyncState::Syncing);
        assert!(NetworkInfo::is_syncing(&network_handle));
        network_handle.update_sync_state(SyncState::Idle);
        assert!(!NetworkInfo::is_syncing(&network_handle));
        network_handle.update_sync_state(SyncState::Syncing);
        assert!(NetworkInfo::is_syncing(&network_handle));

        // wait for all initiator connections
        let mut established = listener0.take(2);
        while let Some(ev) = established.next().await {
            match ev {
                NetworkEvent::SessionEstablished {
                    peer_id,
                    remote_addr,
                    client_version,
                    capabilities,
                    messages,
                    status,
                    version,
                } => {
                    // to insert a new peer in transactions peerset
                    transactions.on_network_event(NetworkEvent::SessionEstablished {
                        peer_id,
                        remote_addr,
                        client_version,
                        capabilities,
                        messages,
                        status,
                        version,
                    })
                }
                NetworkEvent::PeerAdded(_peer_id) => continue,
                ev => {
                    panic!("unexpected event {ev:?}")
                }
            }
        }
        // random tx: <https://etherscan.io/getRawTx?tx=0x9448608d36e721ef403c53b00546068a6474d6cbab6816c3926de449898e7bce>
        let input = hex!("02f871018302a90f808504890aef60826b6c94ddf4c5025d1a5742cf12f74eec246d4432c295e487e09c3bbcc12b2b80c080a0f21a4eacd0bf8fea9c5105c543be5a1d8c796516875710fafafdf16d16d8ee23a001280915021bb446d1973501a67f93d2b38894a514b976e7b46dc2fe54598d76");
        let signed_tx = TransactionSigned::decode(&mut &input[..]).unwrap();
        transactions.on_network_tx_event(NetworkTransactionEvent::IncomingTransactions {
            peer_id: *handle1.peer_id(),
            msg: Transactions(vec![signed_tx.clone()]),
        });
        poll_fn(|cx| {
            let _ = transactions.poll_unpin(cx);
            Poll::Ready(())
        })
        .await;
        assert!(!NetworkInfo::is_initially_syncing(&network_handle));
        assert!(NetworkInfo::is_syncing(&network_handle));
        assert!(!pool.is_empty());
        handle.terminate().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_handle_incoming_transactions() {
        reth_tracing::init_test_tracing();
        let net = Testnet::create(3).await;

        let mut handles = net.handles();
        let handle0 = handles.next().unwrap();
        let handle1 = handles.next().unwrap();

        drop(handles);
        let handle = net.spawn();

        let listener0 = handle0.event_listener();

        handle0.add_peer(*handle1.peer_id(), handle1.local_addr());
        let secret_key = SecretKey::new(&mut rand::thread_rng());

        let client = NoopProvider::default();
        let pool = testing_pool();
        let config = NetworkConfigBuilder::new(secret_key)
            .disable_discovery()
            .listener_port(0)
            .build(client);
        let (network_handle, network, mut transactions, _) = NetworkManager::new(config)
            .await
            .unwrap()
            .into_builder()
            .transactions(pool.clone())
            .split_with_handle();
        tokio::task::spawn(network);

        network_handle.update_sync_state(SyncState::Idle);

        assert!(!NetworkInfo::is_syncing(&network_handle));

        // wait for all initiator connections
        let mut established = listener0.take(2);
        while let Some(ev) = established.next().await {
            match ev {
                NetworkEvent::SessionEstablished {
                    peer_id,
                    remote_addr,
                    client_version,
                    capabilities,
                    messages,
                    status,
                    version,
                } => {
                    // to insert a new peer in transactions peerset
                    transactions.on_network_event(NetworkEvent::SessionEstablished {
                        peer_id,
                        remote_addr,
                        client_version,
                        capabilities,
                        messages,
                        status,
                        version,
                    })
                }
                NetworkEvent::PeerAdded(_peer_id) => continue,
                ev => {
                    panic!("unexpected event {ev:?}")
                }
            }
        }
        // random tx: <https://etherscan.io/getRawTx?tx=0x9448608d36e721ef403c53b00546068a6474d6cbab6816c3926de449898e7bce>
        let input = hex!("02f871018302a90f808504890aef60826b6c94ddf4c5025d1a5742cf12f74eec246d4432c295e487e09c3bbcc12b2b80c080a0f21a4eacd0bf8fea9c5105c543be5a1d8c796516875710fafafdf16d16d8ee23a001280915021bb446d1973501a67f93d2b38894a514b976e7b46dc2fe54598d76");
        let signed_tx = TransactionSigned::decode(&mut &input[..]).unwrap();
        transactions.on_network_tx_event(NetworkTransactionEvent::IncomingTransactions {
            peer_id: *handle1.peer_id(),
            msg: Transactions(vec![signed_tx.clone()]),
        });
        assert_eq!(
            *handle1.peer_id(),
            transactions.transactions_by_peers.get(&signed_tx.hash()).unwrap()[0]
        );

        // advance the transaction manager future
        poll_fn(|cx| {
            let _ = transactions.poll_unpin(cx);
            Poll::Ready(())
        })
        .await;

        assert!(!pool.is_empty());
        assert!(pool.get(&signed_tx.hash).is_some());
        handle.terminate().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_on_get_pooled_transactions_network() {
        reth_tracing::init_test_tracing();
        let net = Testnet::create(2).await;

        let mut handles = net.handles();
        let handle0 = handles.next().unwrap();
        let handle1 = handles.next().unwrap();

        drop(handles);
        let handle = net.spawn();

        let listener0 = handle0.event_listener();

        handle0.add_peer(*handle1.peer_id(), handle1.local_addr());
        let secret_key = SecretKey::new(&mut rand::thread_rng());

        let client = NoopProvider::default();
        let pool = testing_pool();
        let config = NetworkConfigBuilder::new(secret_key)
            .disable_discovery()
            .listener_port(0)
            .build(client);
        let (network_handle, network, mut transactions, _) = NetworkManager::new(config)
            .await
            .unwrap()
            .into_builder()
            .transactions(pool.clone())
            .split_with_handle();
        tokio::task::spawn(network);

        network_handle.update_sync_state(SyncState::Idle);

        assert!(!NetworkInfo::is_syncing(&network_handle));

        // wait for all initiator connections
        let mut established = listener0.take(2);
        while let Some(ev) = established.next().await {
            match ev {
                NetworkEvent::SessionEstablished {
                    peer_id,
                    remote_addr,
                    client_version,
                    capabilities,
                    messages,
                    status,
                    version,
                } => transactions.on_network_event(NetworkEvent::SessionEstablished {
                    peer_id,
                    remote_addr,
                    client_version,
                    capabilities,
                    messages,
                    status,
                    version,
                }),
                NetworkEvent::PeerAdded(_peer_id) => continue,
                ev => {
                    panic!("unexpected event {ev:?}")
                }
            }
        }
        handle.terminate().await;

        let tx = MockTransaction::eip1559();
        let _ = transactions
            .pool
            .add_transaction(reth_transaction_pool::TransactionOrigin::External, tx.clone())
            .await;

        let request = GetPooledTransactions(vec![tx.get_hash()]);

        let (send, receive) = oneshot::channel::<RequestResult<PooledTransactions>>();

        transactions.on_network_tx_event(NetworkTransactionEvent::GetPooledTransactions {
            peer_id: *handle1.peer_id(),
            request,
            response: send,
        });

        match receive.await.unwrap() {
            Ok(PooledTransactions(transactions)) => {
                assert_eq!(transactions.len(), 1);
            }
            Err(e) => {
                panic!("error: {:?}", e);
            }
        }
    }
}
