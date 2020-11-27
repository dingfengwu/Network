//! Test suite for the network functions.

use crate::compute::ComputeNode;
use crate::interfaces::Response;
use crate::test_utils::{Network, NetworkConfig};
use crate::utils::create_valid_transaction;
use futures::future::join_all;
use naom::primitives::block::Block;
use naom::primitives::transaction::Transaction;
use sodiumoxide::crypto::sign;
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use tokio::sync::Barrier;
use tracing::{info, info_span};

#[tokio::test(threaded_scheduler)]
async fn create_block() {
    let _ = tracing_subscriber::fmt::try_init();

    //
    // Arrange
    //
    let network_config = complete_network_config(10000);
    let mut network = Network::create_from_config(&network_config).await;

    let (seed_utxo, _transactions, t_hash, tx) = valid_transactions();
    compute_seed_utxo(&mut network, "compute1", &seed_utxo).await;

    //
    // Act
    //
    tokio::join!(
        task_connect_and_send_payment_to_compute(&mut network, "user1", "compute1", &tx).await,
        compute_handle_event(
            &mut network,
            "compute1",
            "All transactions successfully added to tx pool",
        )
    );
    let block_transaction_before =
        compute_current_block_transactions(&mut network, "compute1").await;
    compute_generate_block(&mut network, "compute1").await;
    let block_transaction_after =
        compute_current_block_transactions(&mut network, "compute1").await;

    //
    // Assert
    //
    assert_eq!(block_transaction_before, None);
    assert_eq!(block_transaction_after, Some(vec![t_hash]));
}

#[tokio::test(threaded_scheduler)]
async fn create_block_raft_1_node() {
    create_block_raft(10200, 1).await;
}

#[tokio::test(threaded_scheduler)]
async fn create_block_raft_2_nodes() {
    create_block_raft(10210, 2).await;
}

#[tokio::test(threaded_scheduler)]
async fn create_block_raft_3_nodes() {
    create_block_raft(10240, 3).await;
}

#[tokio::test(threaded_scheduler)]
async fn create_block_raft_20_nodes() {
    create_block_raft(10340, 20).await;
}

async fn create_block_raft(initial_port: u16, compute_count: usize) {
    let _ = tracing_subscriber::fmt::try_init();

    //
    // Arrange
    //
    let network_config = complete_network_config_with_n_compute_raft(initial_port, compute_count);
    let mut network = Network::create_from_config(&network_config).await;

    let (seed_utxo, _transactions, t_hash, tx) = valid_transactions();
    compute_seed_utxo(&mut network, "compute1", &seed_utxo).await;
    tokio::join!(
        task_connect_and_send_payment_to_compute(&mut network, "user1", "compute1", &tx).await,
        compute_handle_event(
            &mut network,
            "compute1",
            "All transactions successfully added to tx pool",
        )
    );

    //
    // Act
    //
    compute_vote_generate_block(&mut network, "compute1").await;
    let block_transaction_before =
        compute_current_block_transactions(&mut network, "compute1").await;

    compute_raft_group_all_handle_event(
        &mut network,
        &network_config.compute_nodes,
        "Block committed",
    )
    .await;

    let block_transaction_after =
        compute_current_block_transactions(&mut network, "compute1").await;

    //
    // Assert
    //
    assert_eq!(block_transaction_before, None);
    assert_eq!(block_transaction_after, Some(vec![t_hash]));
}

#[tokio::test(threaded_scheduler)]
async fn proof_of_work() {
    let _ = tracing_subscriber::fmt::try_init();

    //
    // Arrange
    //
    let network_config = complete_network_config_with_n_miners(10010, 3);
    let mut network = Network::create_from_config(&network_config).await;

    let block = Block::new();

    //
    // Act
    //
    tokio::join!(
        task_spawn_connect_and_send_pow(&mut network, "miner1", "compute1", &block).await,
        task_spawn_connect_and_send_pow(&mut network, "miner2", "compute1", &block).await,
        task_spawn_connect_and_send_pow(&mut network, "miner3", "compute1", &block).await,
    );

    let block_hash_before = compute_block_hash(&mut network, "compute1").await;
    compute_handle_event(&mut network, "compute1", "Received PoW successfully").await;
    compute_handle_event(&mut network, "compute1", "Received PoW successfully").await;
    compute_handle_event(&mut network, "compute1", "Received PoW successfully").await;
    let block_hash_after = compute_block_hash(&mut network, "compute1").await;

    //
    // Assert
    //
    assert_eq!(block_hash_before.len(), 0);
    assert_eq!(block_hash_after.len(), 64);
}

#[tokio::test(threaded_scheduler)]
async fn send_block_to_storage() {
    let _ = tracing_subscriber::fmt::try_init();

    let network_config = complete_network_config(10020);
    let mut network = Network::create_from_config(&network_config).await;

    tokio::join!(
        {
            let comp = network.compute("compute1").unwrap().clone();
            async move {
                let mut c = comp.lock().await;
                c.current_block = Some(Block::new());
                c.connect_to_storage().await.unwrap();
                let _write_to_store = c.send_block_to_storage().await.unwrap();
            }
        },
        {
            let storage = network.storage("storage1").unwrap().clone();
            async move {
                let mut storage = storage.lock().await;
                match storage.handle_next_event().await {
                    Some(Ok(Response {
                        success: true,
                        reason: "Block received and added",
                    })) => (),
                    other => panic!("Unexpected result: {:?}", other),
                }
            }
        }
    );
}

#[tokio::test(threaded_scheduler)]
async fn receive_payment_tx_user() {
    let _ = tracing_subscriber::fmt::try_init();

    let mut network_config = complete_network_config(10030);
    network_config.user_nodes.push("user2".to_string());
    let mut network = Network::create_from_config(&network_config).await;

    let compute_node_addr = network.get_address("compute1").await.unwrap();
    let user2_addr = network.get_address("user2").await.unwrap();

    tokio::join!(
        {
            let u = network.user("user1").unwrap().clone();
            async move {
                let mut u = u.lock().await;
                u.connect_to(user2_addr).await.unwrap();
                u.connect_to(compute_node_addr).await.unwrap();
                u.amount = 10;

                u.send_address_request(user2_addr).await.unwrap();
            }
        },
        {
            let u2 = network.user("user2").unwrap().clone();
            async move {
                let mut u2 = u2.lock().await;
                match u2.handle_next_event().await {
                    Some(Ok(Response {
                        success: true,
                        reason: "New address ready to be sent",
                    })) => return (),
                    other => panic!("Unexpected result: {:?}", other),
                }
            }
        }
    );
}

async fn compute_handle_event(network: &mut Network, compute: &str, reason_str: &str) {
    let mut c = network.compute(compute).unwrap().lock().await;
    compute_handle_event_for_node(&mut c, reason_str).await;
}

async fn compute_handle_event_for_node(c: &mut ComputeNode, reason_str: &str) {
    match c.handle_next_event().await {
        Some(Ok(Response {
            success: true,
            reason,
        })) if reason == reason_str => (),
        other => panic!("Unexpected result: {:?}", other),
    }
}

async fn compute_raft_group_all_handle_event(
    network: &mut Network,
    compute_group: &[String],
    reason_str: &str,
) {
    let mut join_handles = Vec::new();
    let barrier = Arc::new(Barrier::new(compute_group.len()));
    for compute_name in compute_group {
        let barrier = barrier.clone();
        let compute_name = compute_name.clone();
        let compute = network.compute(&compute_name).unwrap().clone();

        join_handles.push(async move {
            let _peer_span = info_span!("peer", ?compute_name);
            info!("Start wait for event");

            let mut compute = compute.lock().await;
            compute_handle_event_for_node(&mut compute, reason_str).await;

            info!("Start wait for completion of other in raft group");
            let result = tokio::select!(
               _ = barrier.wait() => (),
               _ = compute_handle_event_for_node(&mut compute, "Not an event") => (),
            );

            info!("Stop wait for event: {:?}", result);
        });
    }
    let _ = join_all(join_handles).await;
}

async fn compute_seed_utxo(
    network: &mut Network,
    compute: &str,
    seed_utxo: &BTreeMap<String, Transaction>,
) {
    let c = network.compute(compute).unwrap().lock().await;
    c.seed_utxo_set(seed_utxo.clone());
}

async fn compute_generate_block(network: &mut Network, compute: &str) {
    let mut c = network.compute(compute).unwrap().lock().await;
    c.generate_block();
}

async fn compute_vote_generate_block(network: &mut Network, compute: &str) {
    let mut c = network.compute(compute).unwrap().lock().await;
    c.vote_generate_block().await;
}

async fn compute_block_hash(network: &mut Network, compute: &str) -> String {
    let c = network.compute(compute).unwrap().lock().await;
    c.last_block_hash.clone()
}

async fn compute_current_block_transactions(
    network: &mut Network,
    compute: &str,
) -> Option<Vec<String>> {
    let c = network.compute(compute).unwrap().lock().await;
    c.current_block.as_ref().map(|b| b.transactions.clone())
}

async fn task_connect_and_send_payment_to_compute(
    network: &mut Network,
    from_user: &str,
    to_compute: &str,
    tx: &Transaction,
) -> impl Future<Output = ()> {
    let compute_node_addr = network.get_address(to_compute).await.unwrap().clone();
    let user = network.user(from_user).unwrap();
    let tx = tx.clone();
    let u = user.clone();

    async move {
        let mut u = u.lock().await;
        u.connect_to(compute_node_addr).await.unwrap();
        u.send_payment_to_compute(compute_node_addr, tx)
            .await
            .unwrap();
    }
}

async fn task_spawn_connect_and_send_pow(
    network: &mut Network,
    from_miner: &str,
    to_compute: &str,
    block: &Block,
) -> impl Future<Output = ()> {
    let compute_node_addr = network.get_address(to_compute).await.unwrap();
    let miner = network.miner(from_miner).unwrap();
    let m = miner.clone();
    let miner_block = block.clone();

    async move {
        let mut m = m.lock().await;
        let _conn = m.connect_to(compute_node_addr).await;
        let (pow, transaction) = m.generate_pow_for_block(miner_block).await.unwrap();
        m.send_pow(compute_node_addr, pow, transaction)
            .await
            .unwrap();
    }
}

fn valid_transactions() -> (
    BTreeMap<String, Transaction>,
    BTreeMap<String, Transaction>,
    String,
    Transaction,
) {
    let intial_t_hash = "000000".to_owned();
    let receiver_addr = "000000".to_owned();

    let (pk, sk) = sign::gen_keypair();
    let (t_hash, payment_tx) = create_valid_transaction(&intial_t_hash, &receiver_addr, &pk, &sk);

    let transactions = {
        let mut m = BTreeMap::new();
        m.insert(t_hash.clone(), payment_tx.clone());
        m
    };
    let seed_utxo = {
        let mut m = BTreeMap::new();
        m.insert(intial_t_hash, Transaction::new());
        m
    };
    (seed_utxo, transactions, t_hash, payment_tx)
}

fn complete_network_config(initial_port: u16) -> NetworkConfig {
    NetworkConfig {
        initial_port,
        compute_raft: false,
        miner_nodes: vec!["miner1".to_string()],
        compute_nodes: vec!["compute1".to_string()],
        storage_nodes: vec!["storage1".to_string()],
        user_nodes: vec!["user1".to_string()],
    }
}

fn complete_network_config_with_n_miners(initial_port: u16, miner_count: usize) -> NetworkConfig {
    let mut cfg = complete_network_config(initial_port);
    cfg.miner_nodes = (0..miner_count)
        .map(|idx| format!("miner{}", idx + 1))
        .collect();
    cfg
}

fn complete_network_config_with_n_compute_raft(
    initial_port: u16,
    compute_count: usize,
) -> NetworkConfig {
    let mut cfg = complete_network_config(initial_port);
    cfg.compute_raft = true;
    cfg.compute_nodes = (0..compute_count)
        .map(|idx| format!("compute{}", idx + 1))
        .collect();
    cfg
}
