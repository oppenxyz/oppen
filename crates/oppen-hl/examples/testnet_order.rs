//! P1 gate: a signed testnet order from the CLI.
//!
//! Places a post-only BTC bid 10% below the mid, confirms it is resting via
//! `orderStatus` by cloid, cancels it by cloid, then probes an order with an
//! `expiresAfter` in the past and expects a rejection (`docs/hl-signing.md`,
//! open question 1).
//!
//! ```text
//! OPPEN_TESTNET_AGENT_KEY=0x... OPPEN_TESTNET_USER=0x... \
//!   cargo run -p oppen-hl --example testnet_order
//! ```
//!
//! `OPPEN_TESTNET_USER` is the master (or sub-account) address the agent
//! signs for; set `OPPEN_TESTNET_VAULT` to route through a sub-account.
//! The key is read from the environment only; never paste it anywhere else.

use std::time::{SystemTime, UNIX_EPOCH};

use rust_decimal::Decimal;

use oppen_hl::info::OrderRef;
use oppen_hl::types::OrderStatusResponse;
use oppen_hl::wire::{CancelByCloidWire, Cloid, Grouping, Tif};
use oppen_hl::{
    Action, Address, AgentKey, ExchangeClient, ExchangeRequest, InfoClient, Network,
    NonceAllocator, OrderKind, OrderSpec, Status, Universe,
};

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn random_cloid() -> Cloid {
    let mut bytes = [0u8; 16];
    let seed = now_ms().to_be_bytes();
    bytes[..8].copy_from_slice(&seed);
    bytes[8..].copy_from_slice(&(std::process::id() as u64).to_be_bytes());
    Cloid::from_bytes(bytes)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let key = AgentKey::from_hex(&std::env::var("OPPEN_TESTNET_AGENT_KEY")?)?;
    let user = Address::parse(&std::env::var("OPPEN_TESTNET_USER")?)?;
    let vault = std::env::var("OPPEN_TESTNET_VAULT")
        .ok()
        .map(|v| Address::parse(&v))
        .transpose()?;
    let network = Network::Testnet;
    println!("network  {network:?}");
    println!("agent    {}", key.address());
    println!("user     {user}");

    let info = InfoClient::new(network)?;
    let exchange = ExchangeClient::new(network)?;
    let nonces = NonceAllocator::new();

    let meta = info.meta().await?;
    let universe = Universe::from_meta(&meta)?;
    let btc = universe.get("BTC")?;
    let mids = info.all_mids().await?;
    let mid = *mids.get("BTC").ok_or("no BTC mid")?;
    let state = info.clearinghouse_state(user).await?;
    println!("mid      {mid}");
    println!("equity   {}", state.margin_summary.account_value);

    let cloid = random_cloid();
    let spec = OrderSpec {
        is_buy: true,
        px: mid * Decimal::new(9, 1),
        sz: Decimal::new(12, 0) / (mid * Decimal::new(9, 1)),
        kind: OrderKind::Limit { tif: Tif::Alo },
        reduce_only: false,
        cloid: Some(cloid.clone()),
    };
    let wire = spec.to_wire(btc)?;
    println!(
        "order    a={} p={} s={} cloid={}",
        wire.a,
        wire.p.as_str(),
        wire.s.as_str(),
        cloid.as_str()
    );
    let action = Action::Order {
        orders: vec![wire],
        grouping: Grouping::Na,
        builder: None,
    };
    let request = ExchangeRequest::sign(&key, action, nonces.next(), vault, None, network)?;
    let response = exchange.post(&request).await?;
    println!("place    {:?}", response.statuses);
    let oid = match response.statuses.first() {
        Some(Status::Resting { oid }) => *oid,
        other => return Err(format!("expected resting, got {other:?}").into()),
    };

    let status = info
        .order_status(user, OrderRef::Cloid(cloid.clone()))
        .await?;
    match &status {
        OrderStatusResponse::Order { order } => {
            println!(
                "status   {} oid={} cloid={:?}",
                order.status, order.order.oid, order.order.cloid
            );
            assert_eq!(order.order.oid, oid);
        }
        OrderStatusResponse::UnknownOid => {
            return Err("orderStatus by cloid returned unknownOid".into());
        }
    }

    let cancel = Action::CancelByCloid {
        cancels: vec![CancelByCloidWire {
            asset: btc.index,
            cloid: cloid.clone(),
        }],
    };
    let request = ExchangeRequest::sign(&key, cancel, nonces.next(), vault, None, network)?;
    let response = exchange.post(&request).await?;
    println!("cancel   {:?}", response.statuses);

    let expired_spec = OrderSpec {
        cloid: Some(random_cloid()),
        ..spec
    };
    let action = Action::Order {
        orders: vec![expired_spec.to_wire(btc)?],
        grouping: Grouping::Na,
        builder: None,
    };
    let expires_after = now_ms() - 60_000;
    let request = ExchangeRequest::sign(
        &key,
        action,
        nonces.next(),
        vault,
        Some(expires_after),
        network,
    )?;
    match exchange.post(&request).await {
        Ok(response) => println!(
            "expires  UNEXPECTED accept: {:?} (cancel it by hand)",
            response.statuses
        ),
        Err(e) => println!("expires  rejected as expected: {e}"),
    }

    let rl = info.user_rate_limit(user).await?;
    println!(
        "budget   {}/{} used, cumVlm {}",
        rl.n_requests_used, rl.n_requests_cap, rl.cum_vlm
    );
    Ok(())
}
