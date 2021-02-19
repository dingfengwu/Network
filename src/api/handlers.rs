use crate::api::errors;
use crate::comms_handler::Node;
use crate::interfaces::UserRequest;
use crate::wallet::WalletDb;
use naom::constants::D_DISPLAY_PLACES;
use naom::primitives::asset::TokenAmount;
use serde::{Deserialize, Serialize};
use tracing::error;

/// Information about a wallet to be returned to requester
#[derive(Debug, Clone, Serialize, Deserialize)]
struct WalletInfo {
    running_total: f64,
}

/// Information about a payee to be returned to requester
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PayeeInfo {
    address: String,
}

/// Gets the state of the connected wallet and returns it.
/// Returns a `WalletInfo` struct
pub async fn get_wallet_info(wallet_db: WalletDb) -> Result<impl warp::Reply, warp::Rejection> {
    let fund_store = match wallet_db.get_fund_store_err() {
        Ok(fund) => fund,
        Err(_) => return Err(warp::reject::custom(errors::ErrorCannotAccessWallet)),
    };

    let send_val = WalletInfo {
        running_total: fund_store.running_total.0 as f64 / D_DISPLAY_PLACES,
    };

    Ok(warp::reply::json(&send_val))
}

/// Post a new payment from the connected wallet.
pub async fn make_payment(
    peer: Node,
    address: String,
    amount: TokenAmount,
) -> Result<impl warp::Reply, warp::Rejection> {
    let request = UserRequest::SendPaymentAddress { address, amount };
    if let Err(e) = peer.inject_next_event(peer.address(), request) {
        error!("route:make_payment error: {:?}", e);
        return Err(warp::reject::custom(errors::ErrorCannotUserNode));
    }

    Ok(warp::reply::json(&"Payment processing".to_owned()))
}
