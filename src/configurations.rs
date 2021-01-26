use serde::{Deserialize, Deserializer};
use std::net::SocketAddr;

/// Configuration info for a node
#[derive(Debug, Clone, Deserialize)]
pub struct NodeSpec {
    pub address: SocketAddr,
}

/// Configuration info for a database
#[derive(Debug, Clone, Deserialize)]
pub enum DbMode {
    Live,
    Test(usize),
    InMemory,
}

/// Configuration option for a compute node
#[derive(Debug, Clone, Deserialize)]
pub struct ComputeNodeConfig {
    /// Index of the current node in compute_nodes
    pub compute_node_idx: usize,
    /// All compute nodes addresses
    pub compute_nodes: Vec<NodeSpec>,
    /// All storage nodes addresses: only use first
    pub storage_nodes: Vec<NodeSpec>,
    /// All user nodes addresses
    pub user_nodes: Vec<NodeSpec>,
    /// Whether compute node will use raft or act independently (0)
    pub compute_raft: usize,
    /// Timeout for ticking raft
    pub compute_raft_tick_timeout: usize,
    /// Index of the current node in compute_nodes
    pub compute_transaction_timeout: usize,
    /// Transaction hash to use to seed utxo
    #[serde(deserialize_with = "deserialize_compute_seed_utxo")]
    pub compute_seed_utxo: Vec<(i32, String)>,
    /// Partition full size
    pub compute_partition_full_size: usize,
    /// Minimum miner pool size
    pub compute_minimum_miner_pool_len: usize,
    /// Node's legal jurisdiction
    pub jurisdiction: String,
    /// Node's address sanction list
    pub sanction_list: Vec<String>,
}

/// Configuration option for a storage node
#[derive(Debug, Clone, Deserialize)]
pub struct StorageNodeConfig {
    /// Index of the current node in compute_nodes
    pub storage_node_idx: usize,
    /// Use specific database
    pub storage_db_mode: DbMode,
    /// All compute nodes addresses
    pub compute_nodes: Vec<NodeSpec>,
    /// All storage nodes addresses: only use first
    pub storage_nodes: Vec<NodeSpec>,
    /// All user nodes addresses
    pub user_nodes: Vec<NodeSpec>,
    /// Whether storage node will use raft or act independently (0)
    pub storage_raft: usize,
    /// Timeout for ticking raft
    pub storage_raft_tick_timeout: usize,
    /// Timeout for generating a new block
    pub storage_block_timeout: usize,
}

/// Configuration option for a storage node
#[derive(Debug, Clone, Deserialize)]
pub struct MinerNodeConfig {
    /// Index of the current node in miner_nodes
    pub miner_node_idx: usize,
    /// Use specific database
    pub miner_db_mode: DbMode,
    /// Index of the compute node to use in compute_nodes
    pub miner_compute_node_idx: usize,
    /// All compute nodes addresses
    pub compute_nodes: Vec<NodeSpec>,
    /// All storage nodes addresses: only use first
    pub storage_nodes: Vec<NodeSpec>,
    /// All miner nodes addresses
    pub miner_nodes: Vec<NodeSpec>,
    /// All user nodes addresses
    pub user_nodes: Vec<NodeSpec>,
}

/// Configuration option for a user node
#[derive(Debug, Clone, Deserialize)]
pub struct UserNodeConfig {
    /// Index of the current node in user_addrs
    pub user_node_idx: usize,
    /// Use specific database
    pub user_db_mode: DbMode,
    /// Index of the compute node to use in compute_nodes
    pub user_compute_node_idx: usize,
    /// Peer node index in user_nodes
    pub peer_user_node_idx: usize,
    /// All compute nodes addresses
    pub compute_nodes: Vec<NodeSpec>,
    /// All storage nodes addresses: only use first
    pub storage_nodes: Vec<NodeSpec>,
    /// All miner nodes addresses
    pub miner_nodes: Vec<NodeSpec>,
    /// All peer user nodes addresses
    pub user_nodes: Vec<NodeSpec>,
    /// API port
    pub api_port: u16,
    /// Wallet seeds: "user_idx-tx_hash-amount"
    pub user_wallet_seeds: Vec<Vec<String>>,
}

/// Configuration option for initial transactions for a compute node
#[derive(Debug, Clone, Deserialize)]
pub struct InititalTransactions {
    pub t_hash: String,
    pub receiver_address: String,
}

/// Configuration option for setup of compute node
#[derive(Debug, Clone, Deserialize)]
pub struct ComputeNodeSetup {
    pub compute_initial_transactions: Vec<InititalTransactions>,
}

/// Deserializing function for compute_seed_utxo field
fn deserialize_compute_seed_utxo<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<(i32, String)>, D::Error> {
    let values: Vec<String> = Deserialize::deserialize(deserializer)?;
    Ok(values.iter().map(|s| config_to_out_point(&s)).collect())
}

/// Generating the compute_seed_utxo values from given seed string
fn config_to_out_point(seed: &str) -> (i32, String) {
    let mut it = seed.split('-');

    match (it.next(), it.next()) {
        (Some(h), None) => (1, h.to_string()),
        (Some(n), Some(h)) => (n.parse::<i32>().unwrap(), h.to_string()),
        _ => panic!("invalid seed: {}", seed),
    }
}
