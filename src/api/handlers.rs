use crate::api::errors;
use crate::comms_handler::Node;
use crate::db_utils::SimpleDb;
use crate::interfaces::UserRequest;
use crate::storage::get_blocks_by_num;
use crate::wallet::{EncapsulationData, WalletDb};

use bincode::serialize;
use naom::constants::D_DISPLAY_PLACES;
use naom::primitives::asset::TokenAmount;
use serde::{Deserialize, Serialize};
use sha3::{Digest, Sha3_256};
use sodiumoxide::crypto::box_::PublicKey as PK;
use sodiumoxide::crypto::sealedbox;
use std::collections::BTreeMap;
use std::str;
use std::sync::{Arc, Mutex};
use tracing::error;

/// Private/public keypairs, stored with payment address as key.
/// Values are encrypted
#[derive(Debug, Serialize, Deserialize)]
pub struct Addresses {
    addresses: BTreeMap<String, Vec<u8>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct APIBlockInfo {
    b_num: u64,
    b_hash: String,
    merkle_hash: String,
    previous_hash: String,
    compute_nodes: usize,
    transactions: usize,
}

/// Information about a wallet to be returned to requester
#[derive(Debug, Clone, Serialize, Deserialize)]
struct WalletInfo {
    running_total: f64,
}

/// Information needed for client-side encapsulation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyEncapsulation {
    pub public_key: PK,
}

/// Ciphered data received from client
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncapsulatedData {
    pub ciphered_message: Vec<u8>,
}

/// Encapsulated payment received from client
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncapsulatedPayment {
    pub address: String,
    pub amount: TokenAmount,
    pub passphrase: String,
}

//======= GET HANDLERS =======//

/// Gets the state of the connected wallet and returns it.
/// Returns a `WalletInfo` struct
pub async fn get_wallet_info(wallet_db: WalletDb) -> Result<impl warp::Reply, warp::Rejection> {
    let fund_store = match wallet_db.get_fund_store_err() {
        Ok(fund) => fund,
        Err(_) => return Err(warp::reject::custom(errors::ErrorCannotAccessWallet)),
    };

    let send_val = WalletInfo {
        running_total: fund_store.running_total().0 as f64 / D_DISPLAY_PLACES,
    };

    Ok(warp::reply::json(&send_val))
}

/// Gets all present keys and sends them out for export
pub async fn get_wallet_keypairs(wallet_db: WalletDb) -> Result<impl warp::Reply, warp::Rejection> {
    let known_addr = wallet_db.get_known_addresses();
    let mut addresses = BTreeMap::new();

    for addr in known_addr {
        addresses.insert(addr.clone(), wallet_db.get_address_store_encrypted(&addr));
    }

    Ok(warp::reply::json(&Addresses { addresses }))
}

/// Gets a newly generated payment address
pub async fn get_new_payment_address(
    wallet_db: WalletDb,
) -> Result<impl warp::Reply, warp::Rejection> {
    let (address, _) = wallet_db.generate_payment_address().await;

    Ok(warp::reply::json(&address))
}

/// Gets information needed to encapsulate data
pub async fn get_wallet_encapsulation_data(
    wallet_db: WalletDb,
) -> Result<impl warp::Reply, warp::Rejection> {
    wallet_db
        .generate_encapsulation_data()
        .await
        .map_err(|_| warp::reject::custom(errors::ErrorCannotGenerateEncapsulationData))?;

    let encapsulation_data = wallet_db
        .get_encapsulation_data()
        .await
        .map_err(|_| warp::reject::custom(errors::ErrorCannotAccessEncapsulationData))?;

    let response = KeyEncapsulation {
        public_key: encapsulation_data.public_key,
    };

    Ok(warp::reply::json(&response))
}

//======= POST HANDLERS =======//

/// Post to retrieve block information by number
pub async fn post_block_by_num(
    db: Arc<Mutex<SimpleDb>>,
    block_nums: Vec<u64>,
) -> Result<impl warp::Reply, warp::Rejection> {
    let blocks = get_blocks_by_num(db, block_nums);
    let api_blocks: Vec<APIBlockInfo> = blocks
        .iter()
        .map(|b| {
            let previous_hash = match &b.block.header.previous_hash {
                Some(h) => h.clone(),
                None => String::default(),
            };

            let b_hash = hex::encode(Sha3_256::digest(&serialize(&b.block).unwrap()));

            APIBlockInfo {
                b_hash,
                previous_hash,
                b_num: b.block.header.b_num,
                merkle_hash: b.block.header.merkle_root_hash.clone(),
                compute_nodes: b.mining_tx_hash_and_nonces.len(),
                transactions: b.block.transactions.len(),
            }
        })
        .collect();

    Ok(warp::reply::json(&api_blocks))
}

/// Post to import new keypairs to the connected wallet
pub async fn post_import_keypairs(
    db: WalletDb,
    keypairs: Addresses,
) -> Result<impl warp::Reply, warp::Rejection> {
    for (addr, address_set) in keypairs.addresses.iter() {
        match db
            .save_encrypted_address_to_wallet(addr.clone(), address_set.clone())
            .await
        {
            Ok(_) => {}
            Err(_e) => {
                return Err(warp::reject::custom(
                    errors::ErrorCannotSaveAddressesToWallet,
                ))
            }
        }
    }

    Ok(warp::reply::json(&"Key/s saved successfully".to_owned()))
}

///Post make a new payment from the connected wallet
pub async fn post_make_payment(
    db: WalletDb,
    peer: Node,
    encapsulated_data: EncapsulatedData,
) -> Result<impl warp::Reply, warp::Rejection> {
    let encapsulation_data = db
        .get_encapsulation_data()
        .await
        .map_err(|_| warp::reject::custom(errors::ErrorCannotAccessEncapsulationData))?;

    let EncapsulatedData { ciphered_message } = encapsulated_data;

    let EncapsulationData {
        public_key,
        secret_key,
    } = encapsulation_data;

    let deciphered_message = sealedbox::open(&ciphered_message, &public_key, &secret_key)
        .map_err(|_| warp::reject::custom(errors::ErrorCannotDecryptEncapsulatedData))?;

    let EncapsulatedPayment {
        address,
        amount,
        passphrase,
    } = serde_json::from_slice(&deciphered_message)
        .map_err(|_| warp::reject::custom(errors::ErrorCannotDecryptEncapsulatedData))?;

    let request = match db.test_passphrase(passphrase).await {
        Ok(()) => UserRequest::SendPaymentAddress { address, amount },
        Err(()) => return Err(warp::reject::custom(errors::ErrorInvalidPassphrase)),
    };

    if let Err(e) = peer.inject_next_event(peer.address(), request) {
        error!("route:make_payment error: {:?}", e);
        return Err(warp::reject::custom(errors::ErrorCannotAccessUserNode));
    }

    Ok(warp::reply::json(&"Payment processing".to_owned()))
}
