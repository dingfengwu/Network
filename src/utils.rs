use crate::comms_handler::Node;
use crate::configurations::{UtxoSetSpec, WalletTxSpec};
use crate::constants::{MINING_DIFFICULTY, REWARD_ISSUANCE_VAL};
use crate::hash_block::*;
use crate::interfaces::ProofOfWork;
use crate::wallet::WalletDb;
use bincode::serialize;
use futures::future::join_all;
use naom::constants::TOTAL_TOKENS;
use naom::primitives::{
    asset::{Asset, TokenAmount},
    block::{build_merkle_tree, Block},
    transaction::{OutPoint, Transaction, TxConstructor, TxIn, TxOut},
};
use naom::script::{lang::Script, StackEntry};
use naom::utils::transaction_utils::{
    construct_address, construct_create_tx, construct_payment_tx_ins, construct_tx_core,
    construct_tx_hash, get_tx_out_with_out_point,
};
use sha3::{Digest, Sha3_256};
use sodiumoxide::crypto::secretbox::Key;
use sodiumoxide::crypto::sign;
use sodiumoxide::crypto::sign::ed25519::{PublicKey, SecretKey};
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fs::File;
use std::future::Future;
use std::io::Read;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::task;
use tokio::time::Instant;
use tracing::{trace, warn};

pub type LocalEventSender = MpscTracingSender<LocalEvent>;
pub type LocalEventReceiver = mpsc::Receiver<LocalEvent>;

/// Local command event to nodes
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalEvent {
    CoordinatedShutdown(u64),
    Exit(&'static str),
    Ignore,
}

/// Event response processing
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResponseResult {
    Exit,
    Continue,
}

pub struct MpscTracingSender<T> {
    sender: mpsc::Sender<T>,
}

impl<T> Clone for MpscTracingSender<T> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
        }
    }
}

impl<T> From<mpsc::Sender<T>> for MpscTracingSender<T> {
    fn from(sender: mpsc::Sender<T>) -> Self {
        Self { sender }
    }
}

impl<T> MpscTracingSender<T> {
    pub async fn send(&mut self, value: T, tag: &str) -> Result<(), mpsc::error::SendError<T>> {
        use mpsc::error::SendError;
        use mpsc::error::TrySendError;

        match self.sender.try_send(value) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(value)) => {
                trace!("send_tracing({}) full: waiting", tag);
                let start = Instant::now();
                let result = self.sender.send(value).await;
                let elapsed = Instant::now() - start;
                if elapsed < Duration::from_millis(2) {
                    trace!("send_tracing({}) done: waited({:?})", tag, elapsed);
                } else {
                    warn!("send_tracing({}) done: waited({:?})", tag, elapsed);
                }
                result
            }
            Err(TrySendError::Closed(value)) => Err(SendError(value)),
        }
    }
}

/// A running tasks or end result
#[derive(Debug)]
pub enum RunningTaskOrResult<T> {
    None,
    Running(task::JoinHandle<T>),
    Completed(Result<T, task::JoinError>),
}

impl<T> Default for RunningTaskOrResult<T> {
    fn default() -> Self {
        Self::None
    }
}

impl<T> RunningTaskOrResult<T> {
    /// Wait for the handle to complete or wait forever
    pub async fn wait(&mut self) {
        *self = if let RunningTaskOrResult::Running(task) = self {
            Self::Completed(task.await)
        } else {
            std::future::pending().await
        }
    }

    /// Return completed result or None if no completed task
    pub fn completed_result(&self) -> Option<&Result<T, task::JoinError>> {
        if let Self::Completed(v) = self {
            Some(v)
        } else {
            None
        }
    }
}

/// Channel for low volume local events
pub struct LocalEventChannel {
    pub tx: LocalEventSender,
    pub rx: LocalEventReceiver,
}

impl Default for LocalEventChannel {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel(10);
        Self { tx: tx.into(), rx }
    }
}

impl fmt::Debug for LocalEventChannel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "")
    }
}

/// A trivial error to output
pub struct StringError(pub String);

impl Error for StringError {
    fn description(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for StringError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl fmt::Debug for StringError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Attempts to connect to all peers
///
/// ### Arguments
///
/// * `node`     - Node attempting to connect to peers.
/// * `peers`    - Vec of socket addresses of peers
/// * `close_rx` - Receiver for close event or None to finish when all connected
pub async fn loop_connnect_to_peers_async(
    mut node: Node,
    peers: Vec<SocketAddr>,
    mut close_rx: Option<oneshot::Receiver<()>>,
) {
    loop {
        for peer in node.unconnected_peers(&peers).await {
            trace!(?peer, "Try to connect to");
            if let Err(e) = node.connect_to(peer).await {
                trace!(?peer, ?e, "Try to connect to failed");
            } else {
                trace!(?peer, "Try to connect to succeeded");
            }
        }

        let delay_retry = tokio::time::delay_for(Duration::from_millis(500));
        if let Some(close_rx) = &mut close_rx {
            tokio::select! {
                _ = delay_retry => (),
                _ = close_rx => return,
            };
        } else {
            if node.unconnected_peers(&peers).await.is_empty() {
                return;
            }
            delay_retry.await;
        }
    }
}

/// check connected to all peers
///
/// ### Arguments
///
/// * `node`     - Node attempting to connect to peers.
/// * `peers`    - Vec of socket addresses of peers
pub async fn loop_wait_connnect_to_peers_async(node: Node, peers: Vec<SocketAddr>) {
    while !node.unconnected_peers(&peers).await.is_empty() {
        tokio::time::delay_for(Duration::from_millis(10)).await;
    }
}

/// Gets the locally set list of sanctioned addresses
///
/// ### Arguments
///
/// * `path`         - Path to the sanction list
/// * `jurisdiction` - Jurisdiction to fetch sanctioned addresses for
pub fn get_sanction_addresses(path: String, jurisdiction: &str) -> Vec<String> {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };

    let mut buff = String::new();
    file.read_to_string(&mut buff).unwrap();

    let sancs: serde_json::value::Value = serde_json::from_str(&buff).unwrap();

    match sancs[jurisdiction].as_array() {
        Some(v) => (*v
            .iter()
            .map(|i| i.as_str().unwrap().to_string())
            .collect::<Vec<String>>())
        .to_vec(),
        None => Vec::new(),
    }
}

/// Creates a "fake" transaction to save to the local wallet
/// for testing. The transaction will contain 4 tokens
///
/// NOTE: This is a test util function
/// ### Arguments
///
/// * `wallet_db`    - &WalletDb object. Reference to a wallet database
pub async fn create_and_save_fake_to_wallet(
    wallet_db: &WalletDb,
) -> Result<(), Box<dyn std::error::Error>> {
    let (final_address, address_keys) = wallet_db.generate_payment_address().await;
    let (receiver_addr, _) = wallet_db.generate_payment_address().await;

    let (t_hash, _payment_tx) = create_valid_transaction(
        &"00000".to_owned(),
        0,
        &receiver_addr,
        &address_keys.public_key,
        &address_keys.secret_key,
    );
    let tx_out_p = OutPoint::new(t_hash, 0);
    let payment_to_save = TokenAmount(4000);
    let payments = vec![(tx_out_p.clone(), payment_to_save, final_address)];
    wallet_db
        .save_usable_payments_to_wallet(payments)
        .await
        .unwrap();

    Ok(())
}

/// Computes a key that will be shared from a vector of PoWs
///
/// ### Arguments
///
/// * `p_list` - Vectoor of PoWs
pub fn get_partition_entry_key(p_list: &[ProofOfWork]) -> Key {
    let key_sha_seed: Vec<u8> = p_list
        .iter()
        .flat_map(|e| e.address.as_bytes().iter().chain(&e.nonce))
        .copied()
        .collect();

    use std::convert::TryInto;
    let hashed_key = Sha3_256::digest(&key_sha_seed).to_vec();
    let key_slice: [u8; 32] = hashed_key[..].try_into().unwrap();
    Key(key_slice)
}

/// Address to be used in Proof of Work
///
/// ### Arguments
///
/// * `addr`    - Socket address of used in the proof of work
pub fn format_parition_pow_address(addr: SocketAddr) -> String {
    format!("{}", addr)
}

/// Block to be used in Proof of Work
///
/// ### Arguments
///
/// * `block`    - &Block reference to be used in proof of work
pub fn serialize_block_for_pow(block: &Block) -> Vec<u8> {
    serialize(block).unwrap()
}

/// HashBlock to be used in Proof of Work
///
/// ### Arguments
///
/// * `block`    - &HashBlock reference to be used in proof of work
pub fn serialize_hashblock_for_pow(block: &HashBlock) -> Vec<u8> {
    serialize(block).unwrap()
}

/// Calculates the reward for the next block, to be placed within the coinbase tx
///
/// ### Argeumtsn
///
/// * `current_circulation` - Current circulation of all tokens
pub fn calculate_reward(current_circulation: TokenAmount) -> TokenAmount {
    TokenAmount((TOTAL_TOKENS - current_circulation.0) >> REWARD_ISSUANCE_VAL)
}

/// Gets the total amount of tokens for all present coinbase transactions,
/// assuming that they have all received the same amount of reward
///
/// ### Arguments
///
/// * `coinbase_tx` - Coinbase transactions
pub fn get_total_coinbase_tokens(coinbase_tx: &BTreeMap<String, Transaction>) -> TokenAmount {
    let mut total = TokenAmount(0);
    for tx_ind in coinbase_tx.values() {
        total += tx_ind.outputs.iter().map(|x| x.value.token_amount()).sum();
    }
    total
}

/// Concatenates a merkle hash and a coinbase hash to produce a single hash output
///
/// ### Arguments
///
/// * `merkle_hash` - Merkle hash to concatenate onto
/// * `cb_tx_hash`  - Coinbase transaction hash
pub async fn concat_merkle_coinbase(merkle_hash: &str, cb_tx_hash: &str) -> String {
    let merkle_result = build_merkle_tree(&[merkle_hash.to_string(), cb_tx_hash.to_string()]).await;

    if let Some((merkle_tree, _)) = merkle_result {
        hex::encode(merkle_tree.root())
    } else {
        "".to_string()
    }
}

/// Validate Proof of Work an address with a random number
pub fn validate_pow_for_address(pow: &ProofOfWork, rand_num: &Option<&Vec<u8>>) -> bool {
    let mut pow_body = pow.address.as_bytes().to_vec();
    pow_body.extend(rand_num.iter().flat_map(|r| r.iter()).copied());
    pow_body.extend(&pow.nonce);

    validate_pow(&pow_body)
}

/// Validate Proof of Work for a block with a mining transaction
///
/// ### Arguments
///
/// * `prev_hash`   - The hash of the previous block
/// * `merkle_hash` - The merkle hash (+ coinbase)
/// * `nonce`       - Nonce
pub fn validate_pow_block(prev_hash: &str, merkle_hash: &str, nonce: &[u8]) -> bool {
    let mut pow = nonce.to_owned().to_vec();
    pow.extend_from_slice(merkle_hash.as_bytes());
    pow.extend_from_slice(prev_hash.as_bytes());
    validate_pow(&pow)
}

/// Check the hash of given data reach MINING_DIFFICULTY
///
/// ### Arguments
///
/// * `pow`    - &u8 proof of work
fn validate_pow(pow: &[u8]) -> bool {
    let pow_hash = Sha3_256::digest(pow).to_vec();
    pow_hash[0..MINING_DIFFICULTY].iter().all(|v| *v == 0)
}

/// Get the paiment info from the given transactions
///
/// ### Arguments
///
/// * `txs`   - The transactions
pub fn get_paiments_for_wallet<'a>(
    txs: impl Iterator<Item = (&'a String, &'a Transaction)>,
) -> Vec<(OutPoint, TokenAmount, String)> {
    get_tx_out_with_out_point(txs)
        .map(|(out_p, tx_out)| {
            (
                out_p,
                tx_out.value.token_amount(),
                &tx_out.script_public_key,
            )
        })
        .map(|(out_p, amount, address)| (out_p, amount, address.clone().unwrap()))
        .collect()
}

/// Create a valid transaction from givent info
pub fn create_valid_transaction(
    t_hash_hex: &str,
    prev_n: i32,
    receiver_addr_hex: &str,
    pub_key: &PublicKey,
    secret_key: &SecretKey,
) -> (String, Transaction) {
    create_valid_transaction_with_ins_outs(
        &[(prev_n, t_hash_hex)],
        &[receiver_addr_hex],
        pub_key,
        secret_key,
        TokenAmount(1),
    )
}

/// Creates a valid DDE transaction from given info
pub fn create_valid_create_transaction_with_ins_outs(
    drs: Vec<u8>,
    pub_key: PublicKey,
    secret_key: &SecretKey,
) -> (String, Transaction) {
    let create_tx = construct_create_tx(0, drs, pub_key, secret_key, 1);
    let ct_hash = construct_tx_hash(&create_tx);

    (ct_hash, create_tx)
}

/// Create a valid transaction from given info
pub fn create_valid_transaction_with_ins_outs(
    tx_in: &[(i32, &str)],
    receiver_addr_hexs: &[&str],
    pub_key: &PublicKey,
    secret_key: &SecretKey,
    amount: TokenAmount,
) -> (String, Transaction) {
    let tx_ins = {
        let mut tx_in_cons = Vec::new();
        for (prev_n, t_hash_hex) in tx_in {
            let signable = OutPoint::new(t_hash_hex.to_string(), *prev_n);
            let signable_h = hex::encode(serialize(&signable).unwrap());

            let signature = sign::sign_detached(&signable_h.as_bytes(), &secret_key);
            tx_in_cons.push(TxConstructor {
                t_hash: t_hash_hex.to_string(),
                prev_n: *prev_n,
                signatures: vec![signature],
                pub_keys: vec![*pub_key],
            });
        }

        construct_payment_tx_ins(tx_in_cons)
    };

    let tx_outs = {
        let mut tx_outs = Vec::new();

        for addr in receiver_addr_hexs {
            tx_outs.push(TxOut {
                value: Asset::Token(amount),
                locktime: 0,
                script_public_key: Some(addr.to_string()),
                drs_block_hash: None,
                drs_tx_hash: None,
            });
        }
        tx_outs
    };

    let payment_tx = construct_tx_core(tx_ins, tx_outs);
    let t_hash = construct_tx_hash(&payment_tx);

    (t_hash, payment_tx)
}

/// Get the string to display for genesis TxIn
///
/// ### Arguments
///
/// * `tx`    - The transaction
pub fn get_genesis_tx_in_display(tx: &Transaction) -> &str {
    if let Some(tx_in) = tx.inputs.first() {
        if let Some(StackEntry::Bytes(v)) = tx_in.script_signature.stack.first() {
            return &v;
        }
    }

    &""
}

/// Generate utxo_set transactions from seed info
///
/// ### Arguments
///
/// * `seed`      - Set iterated through to generate the transaction set utxo
/// * `tx_in_str` - String to use as genesis transactions TxIn bytes.
pub fn make_utxo_set_from_seed(
    seed: &UtxoSetSpec,
    tx_in_str: &Option<String>,
) -> BTreeMap<String, Transaction> {
    let mut pk_to_address: BTreeMap<String, String> = BTreeMap::new();
    let genesis_tx_in = tx_in_str.clone().map(|tx_in| {
        let mut script_signature = Script::new();
        script_signature.stack.push(StackEntry::Bytes(tx_in));
        TxIn {
            previous_out: None,
            script_signature,
        }
    });
    seed.iter()
        .map(|(tx_hash, tx_out)| {
            let tx = Transaction {
                outputs: tx_out
                    .iter()
                    .map(|out| {
                        let script_public_key =
                            if let Some(addr) = pk_to_address.get(&out.public_key) {
                                addr.clone()
                            } else {
                                let addr = decode_pub_key_as_address(&out.public_key);
                                pk_to_address.insert(out.public_key.clone(), addr.clone());
                                addr
                            };

                        TxOut::new_amount(script_public_key, out.amount)
                    })
                    .collect(),
                inputs: genesis_tx_in.clone().into_iter().collect(),
                ..Transaction::default()
            };
            (tx_hash.clone(), tx)
        })
        .collect()
}

/// Generate wallet transactions from seed info
///
/// ### Arguments
///
/// * `seed`    - &WalletTxSpec object containing parameters to generate wallet transactions
pub fn make_wallet_tx_info(seed: &WalletTxSpec) -> (OutPoint, PublicKey, SecretKey, TokenAmount) {
    let tx_out_p = decode_wallet_out_point(&seed.out_point);
    let amount = TokenAmount(seed.amount as u64);
    let sk = decode_secret_key(&seed.secret_key);
    let pk = decode_pub_key(&seed.public_key);

    (tx_out_p, pk, sk, amount)
}

/// Decodes a wallet's Outpoint
///
/// ### Arguments
///
/// * `out_point`    - String to be split and decode the wallet OutPoint
pub fn decode_wallet_out_point(out_point: &str) -> OutPoint {
    let mut it = out_point.split('-');
    let n = it.next().unwrap().parse().unwrap();
    let tx_hash = it.next().unwrap().parse().unwrap();
    OutPoint::new(tx_hash, n)
}

/// Decodes the public key as address
///
/// ### Arguments
///
/// * `key`    - key to be decoded to give the public key
pub fn decode_pub_key_as_address(key: &str) -> String {
    construct_address(&decode_pub_key(key))
}

/// Decodes the public key
///
/// ### Arguments
///
/// * `key`    - key to be decoded to give the public key
pub fn decode_pub_key(key: &str) -> PublicKey {
    let key_slice = hex::decode(key).unwrap();
    PublicKey::from_slice(&key_slice).unwrap()
}

/// Decodes a secret key from a given key
///
/// ### Arguments
///
/// * `key`    - key to decoded to give the secret key
pub fn decode_secret_key(key: &str) -> SecretKey {
    let key_slice = hex::decode(key).unwrap();
    SecretKey::from_slice(&key_slice).unwrap()
}

/// Stop listening for connection and disconnect existing ones
/// This will complete sent message in the queues.
///
/// ### Arguments
///
/// * `node_conn` - Node to use for connections
pub async fn shutdown_connections(node_conn: &mut Node) {
    join_all(node_conn.stop_listening().await).await;
    join_all(node_conn.disconnect_all(None).await).await;
}

/// Loop reconnect and test disconnect
///
/// ### Arguments
///
/// * `node_conn`         - Node to use for connections
/// * `addrs_to_connect`  - Addresses to establish connections to
/// * `local_events_tx`   - Channel for local events
pub fn loops_re_connect_disconnect(
    node_conn: Node,
    addrs_to_connect: Vec<SocketAddr>,
    mut local_events_tx: LocalEventSender,
) -> (
    (impl Future<Output = ()>, oneshot::Sender<()>),
    (impl Future<Output = ()>, oneshot::Sender<()>),
) {
    // PERMANENT CONNEXION HANDLING
    let re_connect = {
        let (stop_re_connect_tx, stop_re_connect_rx) = tokio::sync::oneshot::channel::<()>();
        let node_conn = node_conn.clone();
        (
            async move {
                println!("Start connect to requested peers");
                loop_connnect_to_peers_async(node_conn, addrs_to_connect, Some(stop_re_connect_rx))
                    .await;
                println!("Reconnect complete");
            },
            stop_re_connect_tx,
        )
    };

    // TEST DIS-CONNECTION HANDLING
    let disconnect_test = {
        let (stop_re_connect_tx, mut stop_re_connect_rx) = tokio::sync::oneshot::channel::<()>();
        let mut node_conn = node_conn;
        let mut paused = true;
        let mut shutdown_num = None;
        (
            async move {
                println!("Start mode input check");
                loop {
                    tokio::select! {
                        _ = tokio::time::delay_for(Duration::from_millis(100)) => (),
                        _ = &mut stop_re_connect_rx => break,
                    };

                    paused = pause_and_disconnect_on_path(&mut node_conn, paused).await;
                    shutdown_num =
                        shutdown_on_path(&mut node_conn, &mut local_events_tx, shutdown_num).await;
                }
                println!("Complete mode input check");
            },
            stop_re_connect_tx,
        )
    };
    (re_connect, disconnect_test)
}

/// Check disconnect path and pause/disconnect requested connections.
/// Return the new paused state.
///
/// ### Arguments
///
/// * `node_conn`   - Node to use for connections
/// * `paused`      - Current paused state
async fn pause_and_disconnect_on_path(node_conn: &mut Node, paused: bool) -> bool {
    let disconnect = format!("disconnect_{}", node_conn.address().port());
    let path = std::path::Path::new(&disconnect);
    match (paused, path.exists()) {
        (false, true) => {
            let content = std::fs::read_to_string(path).unwrap_or_default();
            let diconnect_addrs: Vec<_> = content
                .split(&[',', '\n'][..])
                .filter_map(|v| v.parse::<SocketAddr>().ok())
                .collect();

            warn!(
                "disconnect from {:?} all {:?}",
                node_conn.address(),
                diconnect_addrs
            );
            node_conn.set_pause_listening(true).await;
            if diconnect_addrs.is_empty() {
                join_all(node_conn.disconnect_all(None).await).await;
            } else {
                join_all(node_conn.disconnect_all(Some(&diconnect_addrs)).await).await;
            }
            true
        }
        (true, false) => {
            node_conn.set_pause_listening(false).await;
            false
        }
        _ => paused,
    }
}

/// Check given shutdown path and trigger coordinated or immediate shutdown.
/// Return the new shutdown block num.
///
/// ### Arguments
///
/// * `node_conn`       - Node to use for connections
/// * `local_events_tx` - Channel for local events
/// * `shutdown_num`    - Current shutdown state
async fn shutdown_on_path(
    node_conn: &mut Node,
    local_events_tx: &mut LocalEventSender,
    shutdown_num: Option<(u64, bool)>,
) -> Option<(u64, bool)> {
    let shutdown_now_one = format!("shutdown_now_{}", node_conn.address().port());
    let shutdown_now_all = "shutdown_now".to_owned();
    let shutdown_coord_one = format!("shutdown_coordinated_{}", node_conn.address().port());
    let shutdown_coord_all = "shutdown_coordinated".to_owned();

    let now = true;
    let path = Some((std::path::Path::new(&shutdown_now_all), now))
        .filter(|(p, _)| p.exists())
        .or_else(|| Some((std::path::Path::new(&shutdown_coord_all), !now)))
        .filter(|(p, _)| p.exists())
        .or_else(|| Some((std::path::Path::new(&shutdown_now_one), now)))
        .filter(|(p, _)| p.exists())
        .or_else(|| Some((std::path::Path::new(&shutdown_coord_one), !now)))
        .filter(|(p, _)| p.exists());

    if let Some((path, is_now)) = path {
        let content = std::fs::read_to_string(path).unwrap_or_default();
        let block_num = content.trim().parse::<u64>().ok().unwrap_or(0);
        let result = Some((block_num, is_now));

        if shutdown_num != result {
            warn!(
                "shutdown from {:?} at block {:?} now={}",
                node_conn.address(),
                block_num,
                is_now
            );

            let event = if is_now {
                LocalEvent::Exit("Shutdown")
            } else {
                LocalEvent::CoordinatedShutdown(block_num)
            };
            if let Err(e) = local_events_tx.send(event, "file_shutdown").await {
                warn!("Cound not send {:?} ({:?})", event, e);
            }
        }

        result
    } else {
        shutdown_num
    }
}
