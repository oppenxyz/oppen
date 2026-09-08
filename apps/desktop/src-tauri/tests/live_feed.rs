//! The console's feed, against the real venue.
//!
//! The unit tests in `src/feed.rs` prove the translation is right for a frame
//! that is handed to it. They cannot prove a frame ever arrives — subscribing
//! the wrong channel name, or naming an interval the venue rejects, produces
//! exactly the same green suite and a console that sits at "—" forever. That
//! failure is the one this file exists to catch, so it opens a real socket.
//!
//! Ignored by default, like every other test here that reaches the network:
//!
//! ```text
//! cargo test -p oppen-desktop --test live_feed -- --ignored --nocapture
//! ```

use std::time::Duration;

use oppen_hl::Network;
use oppen_hl::ws::{Subscription, WsEvent, WsPool, WsPoolConfig};

/// The symbol to watch. Listed on both networks, and busy enough on testnet
/// that a quiet minute is a real failure rather than an ordinary one.
const COIN: &str = "BTC";

/// Long enough for the slowest channel the console subscribes.
///
/// `l2Book` is pushed at a 5.4 s median, so a window shorter than this would
/// make a healthy depth feed look absent. `activeAssetCtx` is ~1 s and `bbo`
/// ~0.11 s, and both land many times over inside it.
const WINDOW: Duration = Duration::from_secs(25);

/// Every channel the console subscribes for the symbol the operator selected,
/// and at least one frame on each.
///
/// `trades` is in here because the forming bar is composed from it: the venue's
/// `candle` channel is the reconcile, not the drive.
///
/// The assertion is per channel rather than "some events arrived": a socket
/// that delivers `bbo` at nine frames a second while `candle` is silently
/// rejected would pass any aggregate count, and the chart is exactly the panel
/// that would then never move.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "hits the public testnet websocket"]
async fn every_channel_the_console_watches_delivers() {
    let (pool, mut events) = WsPool::new(WsPoolConfig {
        network: Network::Testnet,
        ..WsPoolConfig::default()
    })
    .expect("a socket pool");

    for sub in [
        Subscription::ActiveAssetCtx { coin: COIN.into() },
        Subscription::Bbo { coin: COIN.into() },
        Subscription::L2Book { coin: COIN.into() },
        Subscription::Trades { coin: COIN.into() },
        Subscription::Candle {
            coin: COIN.into(),
            interval: "1m".into(),
        },
    ] {
        pool.subscribe(sub).expect("the pool took the subscription");
    }

    let (mut ctx, mut bbo, mut book, mut candle) = (0u32, 0u32, 0u32, 0u32);
    let mut trades = 0u32;
    let mut venue_errors: Vec<String> = Vec::new();
    let deadline = tokio::time::Instant::now() + WINDOW;
    while tokio::time::Instant::now() < deadline {
        let Ok(Some(event)) = tokio::time::timeout_at(deadline, events.recv()).await else {
            break;
        };
        match event.event() {
            WsEvent::ActiveAssetCtx { .. } => ctx += 1,
            WsEvent::Bbo { .. } => bbo += 1,
            WsEvent::L2Book(_) => book += 1,
            WsEvent::Candle(_) => candle += 1,
            WsEvent::Trades { .. } => trades += 1,
            // Recorded and printed, never branched on. Venue message text is
            // display-only (`AGENTS.md` invariant 8 and the conventions
            // section), and "Already subscribed" arrives here as an ordinary
            // duplicate that costs the console nothing. A feed that is really
            // refused is caught below by having delivered no frames, which is
            // the observable behaviour rather than the venue's wording.
            WsEvent::VenueError { message, .. } => venue_errors.push(message.clone()),
            WsEvent::SubscriptionQuarantined { subscription, .. } => {
                panic!("the pool gave up on {}", subscription.key())
            }
            _ => {}
        }
        event.acknowledge();
        if ctx > 0 && bbo > 0 && book > 0 && candle > 0 && trades > 0 {
            break;
        }
    }

    println!("ctx={ctx} bbo={bbo} book={book} candle={candle} trades={trades} in {WINDOW:?}");
    if !venue_errors.is_empty() {
        println!("venue said: {venue_errors:?}");
    }
    assert!(
        ctx > 0,
        "activeAssetCtx delivered nothing: the strip is dead"
    );
    assert!(bbo > 0, "bbo delivered nothing: the spread is dead");
    assert!(book > 0, "l2Book delivered nothing: the ladder is dead");
    assert!(
        candle > 0,
        "candle delivered nothing: the bar is never reconciled"
    );
    // The tape is what actually moves the forming bar, so a silent `trades` is
    // a frozen chart even while every other channel looks healthy.
    assert!(
        trades > 0,
        "trades delivered nothing: the chart cannot move"
    );
}

/// How often each source of a moving price actually delivers.
///
/// The chart's forming bar can come from two places: the venue's own `candle`
/// channel, or a bar composed locally from the `trades` tape. They are not the
/// same product — one is the venue's aggregation on the venue's clock, the
/// other is every print as it happens — and this measures the gap rather than
/// arguing about it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "hits the public testnet websocket"]
async fn candle_and_trade_cadence() {
    let (pool, mut events) = WsPool::new(WsPoolConfig {
        network: Network::Testnet,
        ..WsPoolConfig::default()
    })
    .expect("a socket pool");

    for sub in [
        Subscription::Candle {
            coin: COIN.into(),
            interval: "1m".into(),
        },
        Subscription::Trades { coin: COIN.into() },
    ] {
        pool.subscribe(sub).expect("the pool took the subscription");
    }

    let window = Duration::from_secs(60);
    let started = tokio::time::Instant::now();
    let deadline = started + window;
    let (mut candles, mut trade_frames, mut prints) = (0u32, 0u32, 0u32);
    let mut first_candle: Option<Duration> = None;
    let mut last_candle: Option<Duration> = None;

    while tokio::time::Instant::now() < deadline {
        let Ok(Some(event)) = tokio::time::timeout_at(deadline, events.recv()).await else {
            break;
        };
        let at = started.elapsed();
        match event.event() {
            WsEvent::Candle(_) => {
                candles += 1;
                first_candle.get_or_insert(at);
                last_candle = Some(at);
            }
            WsEvent::Trades { trades, .. } => {
                trade_frames += 1;
                prints += trades.len() as u32;
            }
            _ => {}
        }
        event.acknowledge();
    }

    println!(
        "over {window:?}: candle frames={candles} (first at {first_candle:?}, last at {last_candle:?})"
    );
    println!("               trade frames={trade_frames} carrying {prints} prints");
}
