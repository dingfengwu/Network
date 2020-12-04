//! Test suite for the network functions.

use crate::compute::{ComputeNode, MinedBlock};
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
use tracing::{error_span, info};
use tracing_futures::Instrument;

const SEED_UTXO: [&str; 1] = ["000000"];

#[tokio::test(threaded_scheduler)]
async fn create_block_no_raft() {
    let _ = tracing_subscriber::fmt::try_init();

    //
    // Arrange
    //
    let network_config = complete_network_config(10000);
    let mut network = Network::create_from_config(&network_config).await;
    let (_transactions, t_hash, tx) = valid_transactions();

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
    compute_handle_event(&mut network, "compute1", "Block committed").await;
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
    let compute_nodes = &network_config.compute_nodes;
    let (_transactions, t_hash, tx) = valid_transactions();

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
    let block_transaction_before =
        compute_raft_group_all_current_block_transactions(&mut network, compute_nodes).await;

    compute_raft_group_all_handle_event(&mut network, compute_nodes, "Block committed").await;

    let block_transaction_after =
        compute_raft_group_all_current_block_transactions(&mut network, compute_nodes).await;

    //
    // Assert
    //
    assert_eq!(
        block_transaction_before,
        compute_raft_group_all(compute_nodes, None)
    );
    assert_eq!(
        block_transaction_after,
        compute_raft_group_all(compute_nodes, Some(vec![t_hash]))
    );
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
    compute_set_current_block(&mut network, "compute1", block.clone()).await;

    //
    // Act
    //
    tokio::join!(
        task_spawn_connect_and_send_pow(&mut network, "miner1", "compute1", &block).await,
        task_spawn_connect_and_send_pow(&mut network, "miner2", "compute1", &block).await,
        task_spawn_connect_and_send_pow(&mut network, "miner3", "compute1", &block).await,
    );

    let block_before = compute_mined_block_time(&mut network, "compute1").await;
    compute_handle_event(&mut network, "compute1", "Received PoW successfully").await;
    compute_handle_error(&mut network, "compute1", "Not mining given block").await;
    compute_handle_error(&mut network, "compute1", "Not mining given block").await;
    let block_after = compute_mined_block_time(&mut network, "compute1").await;

    //
    // Assert
    //
    assert_eq!(block_before, None);
    assert_eq!(block_after, Some(0));
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
                c.current_mined_block = Some(MinedBlock {
                    nonce: Vec::new(),
                    block: Block::new(),
                    block_tx: BTreeMap::new(),
                    mining_transaction: Transaction::new(),
                });
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
    compute_handle_event_for_node(&mut c, true, reason_str).await;
}

async fn compute_handle_error(network: &mut Network, compute: &str, reason_str: &str) {
    let mut c = network.compute(compute).unwrap().lock().await;
    compute_handle_event_for_node(&mut c, false, reason_str).await;
}

async fn compute_handle_event_for_node(c: &mut ComputeNode, success_val: bool, reason_val: &str) {
    match c.handle_next_event().await {
        Some(Ok(Response { success, reason }))
            if success == success_val && reason == reason_val =>
        {
            ()
        }
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

        let peer_span = error_span!("peer", ?compute_name);
        join_handles.push(
            async move {
                info!("Start wait for event");

                let mut compute = compute.lock().await;
                compute_handle_event_for_node(&mut compute, true, reason_str).await;

                info!("Start wait for completion of other in raft group");
                let result = tokio::select!(
                   _ = barrier.wait() => (),
                   _ = compute_handle_event_for_node(&mut compute, true, "Not an event") => (),
                );

                info!("Stop wait for event: {:?}", result);
            }
            .instrument(peer_span),
        );
    }
    let _ = join_all(join_handles).await;
}

async fn compute_set_current_block(network: &mut Network, compute: &str, block: Block) {
    let mut c = network.compute(compute).unwrap().lock().await;
    c.set_committed_mining_block(block, BTreeMap::new());
}

async fn compute_mined_block_time(network: &mut Network, compute: &str) -> Option<u32> {
    let c = network.compute(compute).unwrap().lock().await;
    c.current_mined_block
        .as_ref()
        .map(|b| b.block.header.time)
        .clone()
}

async fn compute_raft_group_all_current_block_transactions(
    network: &mut Network,
    compute_group: &[String],
) -> Vec<Option<Vec<String>>> {
    let mut result = Vec::new();
    for compute_name in compute_group {
        let r = compute_current_block_transactions(network, compute_name).await;
        result.push(r);
    }
    result
}

fn compute_raft_group_all<T: Clone>(compute_group: &[String], value: T) -> Vec<T> {
    compute_group.iter().map(|_| value.clone()).collect()
}

async fn compute_current_block_transactions(
    network: &mut Network,
    compute: &str,
) -> Option<Vec<String>> {
    let c = network.compute(compute).unwrap().lock().await;
    c.get_mining_block()
        .as_ref()
        .map(|b| b.transactions.clone())
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

fn valid_transactions() -> (BTreeMap<String, Transaction>, String, Transaction) {
    let intial_t_hash = SEED_UTXO[0];
    let receiver_addr = "000001";

    let (pk, sk) = sign::gen_keypair();
    let (t_hash, payment_tx) = create_valid_transaction(intial_t_hash, receiver_addr, &pk, &sk);

    let transactions = {
        let mut m = BTreeMap::new();
        m.insert(t_hash.clone(), payment_tx.clone());
        m
    };

    (transactions, t_hash, payment_tx)
}

fn complete_network_config(initial_port: u16) -> NetworkConfig {
    NetworkConfig {
        initial_port,
        compute_raft: false,
        miner_nodes: vec!["miner1".to_string()],
        compute_nodes: vec!["compute1".to_string()],
        storage_nodes: vec!["storage1".to_string()],
        user_nodes: vec!["user1".to_string()],
        compute_seed_utxo: SEED_UTXO.iter().map(|v| v.to_string()).collect(),
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
