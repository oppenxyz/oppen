//! The console's live socket (`docs/spec.md` items 31, 32 and 34).
//!
//! Item 34 asks for per-feed status and last-tick timestamps, and the console
//! could answer neither: `account_state` reported `last_tick_ms: None` because
//! nothing here ran a socket, so the staleness overlay was permanently stuck on
//! "never connected" and every number on screen was a REST read taken once at
//! mount. This is the socket that makes those answers true.
//!
//! **The fold is [`oppen_core::feed::FeedSession`]'s, not this module's.** The
//! session already knows what a tick means and what a drop means; the console
//! adds one thing to it — the payloads themselves have to reach the operator's
//! screen, which the MCP path never needed. So the loop here does two things
//! per event and no more: hand it to the session, then emit what the panels
//! draw. Anything that decides something belongs in the core.
//!
//! **The pump is deliberately not used.** [`oppen_core::feed::pump::FeedPump`]
//! owns the reconcile retry, the alert feeds and the quote leases — all three
//! are the agent gateway's concerns, and its `run` loop owns the event stream
//! it would have to share. The console needs freshness and payloads, which is
//! `FeedSession::apply` plus this loop. When the console grows an order path it
//! will need the pump's `reconciled` flag too, and that is the point to move
//! this onto the pump rather than now.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use oppen_core::feed::FeedSession;
use oppen_core::ledger::Ledger;
use oppen_core::market::{BookLevel, MarketRow};
use oppen_hl::Network;
use oppen_hl::types::Level;
use oppen_hl::ws::{Subscription, WsEvent, WsPool, WsPoolConfig};
use tauri::{AppHandle, Emitter, Manager};

/// The single channel the console listens on.
///
/// One event with a tagged payload rather than five, so the frontend has one
/// listener and one place where an unknown variant is ignored — a console that
/// silently stopped drawing because a new variant went to a channel nobody
/// subscribed is the failure this shape rules out.
const CHANNEL: &str = "feed://update";

/// What the console draws, as it arrives.
///
/// Prices stay strings the whole way across, like every other price on this
/// boundary: the renderer parses at its own edge, and nothing between the venue
/// and the pixel rounds.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FeedUpdate {
    /// `activeAssetCtx`, ~1 s. The rail row, and the strip that reads it.
    ///
    /// A whole [`MarketRow`], not the three fields the strip shows: the frame
    /// carries everything the REST universe read carries, so the console
    /// replaces the row rather than patching fields into it — and the rail,
    /// which renders the same row, goes live for the watched symbol for free.
    Ctx { at_ms: u64, row: MarketRow },
    /// `bbo`, ~0.11 s. Top of book, and the spread the strip shows.
    ///
    /// Either side is `None` when that side is empty, which
    /// `docs/specs/fair-value.md` §5.2 requires to read as stale rather than
    /// as zero — so the frontend must render an absent side as "—".
    Bbo {
        coin: String,
        at_ms: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        bid: Option<BookLevel>,
        #[serde(skip_serializing_if = "Option::is_none")]
        ask: Option<BookLevel>,
    },
    /// `l2Book`, ~5.4 s median. The depth ladder.
    Book {
        coin: String,
        at_ms: u64,
        bids: Vec<BookLevel>,
        asks: Vec<BookLevel>,
    },
    /// `candle`. The forming bar, which is the only one that moves.
    Candle {
        coin: String,
        interval: String,
        time_ms: u64,
        open: String,
        high: String,
        low: String,
        close: String,
        volume: String,
    },
    /// The public tape. Every print, as it happens.
    ///
    /// **This is what moves the forming bar**, not the `candle` channel. The
    /// venue aggregates candles on its own clock — measured on testnet BTC at
    /// eight frames a minute with a seventeen-second tail carrying none — so a
    /// chart driven only by that channel sits still through prints the operator
    /// can see on the tape beside it. The bar is composed here optimistically
    /// and corrected when the venue's own bar arrives, which is the same
    /// optimistic-first / reconcile shape the rest of the real-time surface
    /// uses.
    Trade {
        coin: String,
        at_ms: u64,
        /// The last print in the frame, which sets the bar's close.
        px: String,
        /// The frame's own extremes. **Not derivable from `px`**: the venue
        /// batches the tape, so a frame can carry a spike in the middle and a
        /// bar built from the last print alone would miss the high the market
        /// actually traded — wrong until the venue's next candle corrects it.
        high: String,
        low: String,
        /// Every print in the frame, summed.
        sz: String,
    },
    /// The socket's own state, so item 34's overlay has something to read.
    ///
    /// Emitted on connect, drop and resume rather than per tick: a status event
    /// beside every `bbo` frame would be nine messages a second saying the same
    /// thing. Freshness between these is the `at_ms` the payload events carry.
    Status {
        #[serde(skip_serializing_if = "Option::is_none")]
        last_tick_ms: Option<u64>,
        connected: bool,
        /// The venue's own words when a socket dropped, display-only.
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
}

fn level(from: &Level) -> BookLevel {
    BookLevel {
        px: from.px.to_string(),
        sz: from.sz.to_string(),
        n: from.n,
    }
}

/// One network's live feeds, and the subscriptions the console is holding.
pub struct ConsoleFeed {
    network: Network,
    session: Arc<FeedSession>,
    pool: WsPool,
    /// What the operator is looking at. Swapped whole on every selection, so
    /// the console never holds a feed for a symbol it stopped drawing.
    watching: Mutex<Vec<Subscription>>,
}

impl ConsoleFeed {
    /// Open the socket for one network and start folding its events.
    ///
    /// The account channels are subscribed only when an account is configured.
    /// Market data does not need one, and a console with no account still has
    /// a chart to draw — item 34's status is about the socket, not the wallet.
    pub fn start(
        app: &AppHandle,
        network: Network,
        account: Option<String>,
    ) -> Result<Self, String> {
        let ledger = Arc::new(
            Ledger::open_at(
                &data_dir(app)?.join(oppen_core::db_file_name(network)),
                network,
            )
            .map_err(|e| format!("ledger: {e}"))?,
        );
        let session = Arc::new(FeedSession::new());
        let (pool, mut events) = WsPool::new(WsPoolConfig {
            network,
            ..WsPoolConfig::default()
        })
        .map_err(|e| format!("socket pool: {e}"))?;

        if let Some(user) = account.as_deref().and_then(|a| a.parse().ok()) {
            // Fills and order transitions, so the activity stream and the
            // ledger see what the account did while the console was open.
            let _ = pool.subscribe(Subscription::UserFills { user });
            let _ = pool.subscribe(Subscription::OrderUpdates { user });
        }

        let handle = app.clone();
        let loop_session = Arc::clone(&session);
        let loop_account = account.unwrap_or_default();
        tauri::async_runtime::spawn(async move {
            while let Some(event) = events.recv().await {
                // The session first: it is what decides, and a payload emitted
                // before the fold would let the screen lead the state the
                // guardrails read.
                //
                // The error is surfaced, never discarded. `FeedSession::apply`
                // reports a fill it could not write precisely so the caller can
                // decide, and the ledger is append-only and hash-chained
                // (invariant 7): a row that did not land is a chain missing a
                // fill, and an operator watching a console that says nothing
                // would go on trading against a position oppen has mis-stated.
                // The console cannot mark the session unreconciled itself —
                // that is the pump's — so the least it must do is say so.
                if let Err(error) = loop_session.apply(&ledger, &loop_account, &event, now_ms()) {
                    let _ = handle.emit(
                        CHANNEL,
                        FeedUpdate::Status {
                            last_tick_ms: loop_session.state().last_tick_ms,
                            connected: true,
                            detail: Some(format!("a fill could not be recorded: {error}")),
                        },
                    );
                }
                if let Some(update) = translate(&event, loop_session.state().last_tick_ms) {
                    let _ = handle.emit(CHANNEL, update);
                }
            }
        });

        Ok(ConsoleFeed {
            network,
            session,
            pool,
            watching: Mutex::new(Vec::new()),
        })
    }

    /// Whether this feed already serves the network being asked about.
    pub fn serves(&self, network: Network) -> bool {
        self.network == network
    }

    /// The freshness `account_state` reports (item 34).
    pub fn last_tick_ms(&self) -> Option<u64> {
        self.session.state().last_tick_ms
    }

    /// Point the socket at the symbol the operator selected.
    ///
    /// Five channels because they answer five different questions at five
    /// different cadences, and the panels want all of them: `activeAssetCtx`
    /// for the strip, `bbo` for the spread, `l2Book` for the ladder, `trades`
    /// for the bar as it forms, and `candle` to correct that bar against the
    /// venue's own aggregation. Everything held for the previous symbol is given
    /// back in the same pass — a console that accumulated subscriptions as the
    /// operator browsed would walk into the venue's per-IP ceiling.
    pub fn watch(&self, coin: &str, interval: &str) -> Result<(), String> {
        let wanted = vec![
            Subscription::ActiveAssetCtx { coin: coin.into() },
            Subscription::Bbo { coin: coin.into() },
            Subscription::L2Book { coin: coin.into() },
            Subscription::Trades { coin: coin.into() },
            Subscription::Candle {
                coin: coin.into(),
                interval: interval.into(),
            },
        ];
        let mut held = self.watching.lock().map_err(|_| "feed lock poisoned")?;
        // Recorded as each one is taken, never assumed. Returning early on a
        // refusal without writing down what already succeeded would leak those
        // subscriptions permanently: nothing else knows the pool is holding
        // them, so nothing would ever give them back.
        let mut refused = None;
        for sub in &wanted {
            if held.contains(sub) {
                continue;
            }
            match self.pool.subscribe(sub.clone()) {
                Ok(()) => held.push(sub.clone()),
                Err(error) => {
                    refused = Some(format!("{}: {error}", sub.key()));
                    break;
                }
            }
        }
        // Released after the new ones are asked for, so switching symbols does
        // not leave the console with no feed at all if a subscribe is refused —
        // and released even when one was, because the symbol the operator left
        // is not coming back on screen either way.
        held.retain(|sub| {
            if wanted.contains(sub) {
                return true;
            }
            let _ = self.pool.unsubscribe(sub);
            false
        });
        match refused {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

/// Where the ledger lives.
///
/// `OPPEN_DATA_DIR` first, because that is what `oppen-mcp`'s gateway reads and
/// the two must land on the same file when they are pointed at the same
/// account — R4 makes the database per network, not per process.
fn data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = match std::env::var("OPPEN_DATA_DIR") {
        Ok(dir) => PathBuf::from(dir),
        Err(_) => app
            .path()
            .app_data_dir()
            .map_err(|e| format!("no app data directory: {e}"))?,
    };
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(dir)
}

/// What the panels draw, or `None` for an event that carries nothing to draw.
///
/// Matched explicitly rather than with a wildcard, for the reason
/// `FeedSession::apply` gives: a variant this console starts receiving should
/// be a decision about what it draws, not a silent no-op.
fn translate(event: &WsEvent, last_tick_ms: Option<u64>) -> Option<FeedUpdate> {
    match event {
        WsEvent::ActiveAssetCtx {
            coin,
            ctx,
            received_at_ms,
        } => Some(FeedUpdate::Ctx {
            at_ms: *received_at_ms,
            row: MarketRow::of(coin, ctx),
        }),
        WsEvent::Bbo {
            coin,
            venue_time_ms,
            bid,
            ask,
        } => Some(FeedUpdate::Bbo {
            coin: coin.clone(),
            at_ms: *venue_time_ms,
            bid: bid.as_ref().map(level),
            ask: ask.as_ref().map(level),
        }),
        WsEvent::L2Book(book) => Some(FeedUpdate::Book {
            coin: book.coin.clone(),
            at_ms: book.time,
            bids: book.bids().iter().map(level).collect(),
            asks: book.asks().iter().map(level).collect(),
        }),
        WsEvent::Candle(candle) => Some(FeedUpdate::Candle {
            coin: candle.s.clone(),
            interval: candle.i.clone(),
            time_ms: candle.t,
            open: candle.o.to_string(),
            high: candle.h.to_string(),
            low: candle.l.to_string(),
            close: candle.c.to_string(),
            volume: candle.v.to_string(),
        }),
        WsEvent::Disconnected(dropped) => Some(FeedUpdate::Status {
            last_tick_ms,
            connected: false,
            detail: Some(dropped.reason.clone()),
        }),
        WsEvent::Reconnected(_) => Some(FeedUpdate::Status {
            last_tick_ms,
            connected: true,
            detail: None,
        }),
        // A quarantined feed is one subscription down, not the socket, and the
        // operator has to fix it — so it reports as a status with the venue's
        // own words rather than as a disconnect.
        WsEvent::SubscriptionQuarantined { subscription, .. } => Some(FeedUpdate::Status {
            last_tick_ms,
            connected: true,
            detail: Some(format!("{} is not being resubscribed", subscription.key())),
        }),
        WsEvent::VenueError { message, .. } => Some(FeedUpdate::Status {
            last_tick_ms,
            connected: true,
            detail: Some(message.clone()),
        }),
        // The account channels reach the screen through `account_state`, which
        // reads the ledger this loop has already written. Emitting them here
        // too would give the activity stream two sources for one fill.
        WsEvent::UserFills { .. } | WsEvent::OrderUpdates { .. } => None,
        // One update per frame, not per print: the venue batches the tape and
        // the bar folds the whole batch the same way either way, so a frame
        // carrying twenty prints costs one message to the renderer instead of
        // twenty. The last print in the frame is the one that sets the close.
        WsEvent::Trades { coin, trades } => trades.last().map(|last| {
            // One pass for the three facts a bar needs from a batch. Folded
            // from the first print rather than summed from a typed zero, which
            // would mean naming `rust_decimal` here — a dependency this crate
            // does not have and does not need for one addition.
            let (high, low, volume) = trades.iter().skip(1).fold(
                (trades[0].px, trades[0].px, trades[0].sz),
                |(high, low, volume), trade| {
                    (high.max(trade.px), low.min(trade.px), volume + trade.sz)
                },
            );
            FeedUpdate::Trade {
                coin: coin.clone(),
                at_ms: last.time,
                px: last.px.to_string(),
                high: high.to_string(),
                low: low.to_string(),
                sz: volume.to_string(),
            }
        }),
        // Carries no price, and the pool's own staleness bookkeeping already
        // reports a frame it could not read.
        WsEvent::MessageDropped { .. } => None,
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oppen_hl::types::{AssetCtx, Candle, L2Book};

    fn ctx() -> AssetCtx {
        serde_json::from_value(serde_json::json!({
            "funding": "0.0000125",
            "openInterest": "1234.5",
            "prevDayPx": "100.0",
            "dayNtlVlm": "9999.0",
            "premium": "0.0001",
            "oraclePx": "101.0",
            "markPx": "101.5",
            "midPx": "101.4",
            "impactPxs": ["101.3", "101.6"]
        }))
        .expect("asset context fixture")
    }

    #[test]
    fn a_context_frame_carries_the_strip_the_console_draws() {
        let event = WsEvent::ActiveAssetCtx {
            coin: "BTC".into(),
            ctx: Box::new(ctx()),
            received_at_ms: 1_788_000_000_000,
        };
        let Some(FeedUpdate::Ctx { at_ms, row }) = translate(&event, None) else {
            panic!("a context frame must reach the strip");
        };
        assert_eq!(row.symbol, "BTC");
        assert_eq!(at_ms, 1_788_000_000_000);
        // The venue's own decimal, not a float that has been through an f64.
        assert_eq!(row.mark_px, "101.5");
        assert_eq!(row.open_interest, "1234.5");
        // The same hour-to-date bps rule the REST rail uses: 0.0000125 of
        // price is 0.125 bp. A socket row that disagreed with a REST row would
        // make the rail flicker between two conventions.
        assert_eq!(row.funding_1h_bps, "0.1250");
    }

    #[test]
    fn an_empty_book_side_stays_absent_rather_than_zero() {
        let event = WsEvent::Bbo {
            coin: "ETH".into(),
            venue_time_ms: 42,
            bid: Some(Level {
                px: "10".parse().expect("a price"),
                sz: "2".parse().expect("a size"),
                n: 3,
            }),
            ask: None,
        };
        let Some(FeedUpdate::Bbo { bid, ask, .. }) = translate(&event, None) else {
            panic!("a bbo frame must reach the strip");
        };
        assert_eq!(bid.expect("the bid side").px, "10");
        // §5.2: an empty side is stale, never zero. A `Some(0)` here would let
        // the spread render as a number the venue never quoted.
        assert!(ask.is_none());
    }

    #[test]
    fn a_drop_reports_the_last_tick_it_had_rather_than_clearing_it() {
        let event = WsEvent::Disconnected(Box::new(oppen_hl::ws::Disconnected {
            connection: oppen_hl::ws::ConnectionId::new(0),
            at_ms: 900,
            last_message_ms: Some(800),
            subscriptions: Vec::new(),
            unacked: Vec::new(),
            reason: "1006".into(),
        }));
        let Some(FeedUpdate::Status {
            last_tick_ms,
            connected,
            detail,
        }) = translate(&event, Some(800))
        else {
            panic!("a drop must reach the overlay");
        };
        assert!(!connected);
        // Item 34 wants "how stale", so the last good tick survives the drop.
        assert_eq!(last_tick_ms, Some(800));
        assert_eq!(detail.as_deref(), Some("1006"));
    }

    #[test]
    fn a_fill_does_not_reach_the_screen_twice() {
        let event = WsEvent::OrderUpdates {
            user: "0x0000000000000000000000000000000000000001"
                .parse()
                .expect("an address"),
            updates: Vec::new(),
        };
        // The ledger is the one event system (D6): the activity stream reads it
        // through `account_state`, so emitting here would be a second source.
        assert!(translate(&event, None).is_none());
    }

    #[test]
    fn the_forming_bar_keeps_the_venue_bucket_boundary() {
        let candle: Candle = serde_json::from_value(serde_json::json!({
            "t": 1_788_000_000_000u64,
            "T": 1_788_000_059_999u64,
            "s": "SOL", "i": "1m",
            "o": "1.0", "c": "1.5", "h": "1.6", "l": "0.9", "v": "10", "n": 4
        }))
        .expect("a candle fixture");
        let Some(FeedUpdate::Candle {
            coin,
            interval,
            time_ms,
            close,
            ..
        }) = translate(&WsEvent::Candle(Box::new(candle)), None)
        else {
            panic!("a candle frame must reach the chart");
        };
        assert_eq!(coin, "SOL");
        assert_eq!(interval, "1m");
        // The bucket start, not the arrival instant: the chart draws it into
        // the bar it belongs to.
        assert_eq!(time_ms, 1_788_000_000_000);
        assert_eq!(close, "1.5");
    }

    #[test]
    fn a_depth_frame_keeps_the_venue_side_order() {
        let book: L2Book = serde_json::from_value(serde_json::json!({
            "coin": "BTC",
            "time": 7,
            "levels": [
                [{"px": "100", "sz": "1", "n": 1}, {"px": "99", "sz": "2", "n": 2}],
                [{"px": "101", "sz": "3", "n": 3}]
            ]
        }))
        .expect("a book fixture");
        let Some(FeedUpdate::Book { bids, asks, .. }) =
            translate(&WsEvent::L2Book(Box::new(book)), None)
        else {
            panic!("a book frame must reach the ladder");
        };
        // levels[0] is bids best-first, levels[1] asks. Swapping them would
        // draw the ladder inside out.
        assert_eq!(bids.len(), 2);
        assert_eq!(bids[0].px, "100");
        assert_eq!(asks.len(), 1);
        assert_eq!(asks[0].px, "101");
    }
}
