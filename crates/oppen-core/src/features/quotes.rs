//! The latest `bbo` per symbol, and which symbols are worth holding a socket
//! open for.
//!
//! `micro_tilt_bps` has to come from the `bbo` channel: `docs/specs/
//! fair-value.md` §14.4 correction 4 measured `l2Book` at a 5.4 s median push
//! against the 2 s threshold `micro` is defined by, so a tilt taken from the
//! depth ladder is stale by construction. But `bbo` is a websocket channel and
//! `get_features` is a synchronous read, so something has to hold the last
//! frame between the socket and the tool. This is that.
//!
//! **The lifecycle is the hard part, and it is not the alert lifecycle.** An
//! armed alert is long-lived and says plainly which symbols matter until it
//! fires. A `get_features` call is over in milliseconds, and an agent sweeping
//! the universe would otherwise leave one subscription per symbol it ever
//! looked at, up to the venue's per-IP ceiling. So demand here is a *lease*:
//! asking for a symbol renews it, the pump subscribes what is leased, and a
//! lease nobody has renewed within [`LEASE_TTL`] expires and gives the socket
//! back.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use oppen_hl::types::Bbo;
use rust_decimal::Decimal;

/// How long a symbol stays subscribed after the last request for it.
///
/// Long enough that an agent working through a watchlist and coming back to
/// the first symbol still finds it warm, short enough that a one-off sweep
/// does not hold sockets for the rest of the session. `bbo` costs one
/// subscription slot against the per-IP budget and no request budget at all,
/// so the cost of being generous here is a slot, not a rate limit.
pub const LEASE_TTL: Duration = Duration::from_secs(300);

/// How long a caller will wait for the first frame on a newly leased symbol.
///
/// `bbo` pushes at 0.10–0.13 s when the market is moving, so this is many
/// multiples of the normal wait — but the venue only emits when the BBO
/// *changes*, and a quiet symbol may not print at all. That is why waiting has
/// a bound and expiry is not an error: the tool answers with
/// `micro_tilt_bps: null` and the lease stays, so the next call has it.
pub const WARMUP_WAIT: Duration = Duration::from_millis(1_500);

/// How long a symbol's volatility reading is reused before it is measured
/// again.
///
/// σ over a day of hourly bars barely moves minute to minute, and the fetch
/// behind it is 24 candles per symbol. Five minutes keeps `get_state` from
/// paying for a fresh measurement on every call while staying far shorter than
/// the window it describes.
///
/// [`Volatility::vol_ratio`] is the faster half and turns over about a
/// twelfth of its bars in five minutes, so it does not inherit that argument
/// — it inherits the TTL anyway, because it is cached in the same entry and
/// the alternative is fetching both legs five times as often to sharpen a
/// correction whose competition is a twenty-four-hour lag. Five minutes late
/// is the residue; twenty-four hours late was the problem.
pub const SIGMA_TTL: Duration = Duration::from_secs(300);

/// What the guardrail path needs to know about how much a symbol moves.
///
/// The two halves are measured from the same pair of candle series in one
/// pass, so they are cached together and can never describe different
/// moments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Volatility {
    /// Daily σ as a fraction of price — `0.04` is a coin that moves 4% a day.
    pub sigma_day: Decimal,
    /// The last hour's realised vol against what `sigma_day` implies for one
    /// hour. `None` when the minute bars behind it were unavailable, which
    /// leaves the guardrail's cap untightened rather than refused.
    pub vol_ratio: Option<Decimal>,
}

/// Volatility per symbol.
///
/// Separate from [`QuoteCache`] because it is a different lifecycle: a quote is
/// worthless the moment it is stale and is pushed by a socket, while a σ is
/// pulled on demand and stays true for minutes. Nothing subscribes anything for
/// this — a miss is a fetch by whoever asked.
#[derive(Debug, Default)]
pub struct SigmaCache {
    inner: Mutex<BTreeMap<String, (Volatility, u64)>>,
}

impl SigmaCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The cached reading for `symbol`, or `None` when there is none or it is
    /// stale.
    pub fn get(&self, symbol: &str, now_ms: u64) -> Option<Volatility> {
        let ttl_ms = SIGMA_TTL.as_millis() as u64;
        self.lock()
            .get(symbol)
            .filter(|(_, at)| now_ms.saturating_sub(*at) < ttl_ms)
            .map(|(volatility, _)| *volatility)
    }

    /// Record a freshly measured reading.
    pub fn put(&self, symbol: &str, volatility: Volatility, now_ms: u64) {
        self.lock().insert(symbol.to_owned(), (volatility, now_ms));
    }

    /// See [`QuoteCache::lock`] for why a poisoned lock is taken back.
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, (Volatility, u64)>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Latest quotes, and the leases that keep them coming.
#[derive(Debug, Default)]
pub struct QuoteCache {
    inner: Mutex<BTreeMap<String, Entry>>,
    /// Raised when the leased set changes, so the pump subscribes without
    /// waiting for a tick on a feed nobody has subscribed — the deadlock
    /// `docs/decisions.md` G4 names, in the same shape.
    leased_changed: tokio::sync::Notify,
    /// Raised when any quote lands, so a caller warming a symbol wakes on the
    /// first frame rather than sleeping out its whole budget.
    quote_arrived: tokio::sync::Notify,
}

#[derive(Debug, Default)]
struct Entry {
    quote: Option<Bbo>,
    /// When the lease was last renewed, ms. `None` means nothing has asked for
    /// this symbol — the entry is a quote that arrived for a lease since
    /// expired, and it is not worth a subscription.
    leased_at_ms: Option<u64>,
}

impl QuoteCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take or renew a lease on `symbol`, and return the quote if one is held.
    ///
    /// Renewing on read is what makes the TTL mean "since anybody last cared"
    /// rather than "since the first request", so a symbol an agent polls stays
    /// warm and one it looked at once does not.
    pub fn lease(&self, symbol: &str, now_ms: u64) -> Option<Bbo> {
        let (quote, is_new) = {
            let mut inner = self.lock();
            let entry = inner.entry(symbol.to_owned()).or_default();
            let is_new = entry.leased_at_ms.is_none();
            entry.leased_at_ms = Some(now_ms);
            (entry.quote.clone(), is_new)
        };
        if is_new {
            self.leased_changed.notify_one();
        }
        quote
    }

    /// The latest quote for `symbol` without touching its lease.
    pub fn peek(&self, symbol: &str) -> Option<Bbo> {
        self.lock()
            .get(symbol)
            .and_then(|entry| entry.quote.clone())
    }

    /// Record a frame the socket delivered.
    pub fn observe(&self, quote: Bbo) {
        {
            let mut inner = self.lock();
            let coin = quote.coin.clone();
            inner.entry(coin).or_default().quote = Some(quote);
        }
        self.quote_arrived.notify_waiters();
    }

    /// Symbols whose lease is still live, in a stable order.
    ///
    /// Expired leases are dropped here rather than on a timer of their own:
    /// the pump asks for this list on every pass, so the sweep runs exactly as
    /// often as somebody is in a position to act on it.
    pub fn leased(&self, now_ms: u64) -> Vec<String> {
        let ttl_ms = LEASE_TTL.as_millis() as u64;
        let mut inner = self.lock();
        let mut live = Vec::new();
        for (symbol, entry) in inner.iter_mut() {
            match entry.leased_at_ms {
                Some(at) if now_ms.saturating_sub(at) < ttl_ms => live.push(symbol.clone()),
                // Expired. The quote is kept — it costs nothing and answers a
                // later `peek` with something stale but stamped — while the
                // lease is dropped so the socket goes back.
                Some(_) => entry.leased_at_ms = None,
                None => {}
            }
        }
        live
    }

    /// Wait until the leased set changes. Cancel-safe.
    pub async fn leased_changed(&self) {
        self.leased_changed.notified().await;
    }

    /// Wait up to [`WARMUP_WAIT`] for a quote on `symbol`.
    ///
    /// Registers before checking, so a frame landing between the check and the
    /// wait is not missed — the lost-wakeup this would otherwise have.
    pub async fn warm(&self, symbol: &str) -> Option<Bbo> {
        let deadline = tokio::time::Instant::now() + WARMUP_WAIT;
        loop {
            let waiting = self.quote_arrived.notified();
            if let Some(quote) = self.peek(symbol) {
                return Some(quote);
            }
            if tokio::time::timeout_at(deadline, waiting).await.is_err() {
                return None;
            }
        }
    }

    /// A poisoned lock means a previous caller panicked. The cache holds
    /// quotes and lease stamps and no half-applied state, so the guard is
    /// taken back rather than turning an unrelated panic into a feature tool
    /// that never answers again.
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Entry>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oppen_hl::types::Level;
    use rust_decimal::Decimal;

    const NOW: u64 = 1_756_000_000_000;

    fn quote(coin: &str) -> Bbo {
        let level = Level {
            px: Decimal::ONE,
            sz: Decimal::ONE,
            n: 1,
        };
        Bbo {
            coin: coin.into(),
            time: NOW,
            bbo: [Some(level.clone()), Some(level)],
        }
    }

    #[test]
    fn a_symbol_nobody_asked_for_is_not_leased() {
        let cache = QuoteCache::new();
        cache.observe(quote("BTC"));
        assert!(
            cache.leased(NOW).is_empty(),
            "a quote is not a reason to hold a socket"
        );
    }

    #[test]
    fn leasing_a_symbol_puts_it_on_the_subscribe_list() {
        let cache = QuoteCache::new();
        assert_eq!(cache.lease("BTC", NOW), None, "nothing held yet");
        assert_eq!(cache.leased(NOW), ["BTC"]);
    }

    /// The lease is what the pump subscribes, so it has to expire or an agent
    /// sweeping the universe holds a socket per symbol it ever looked at.
    #[test]
    fn a_lease_nobody_renews_expires() {
        let cache = QuoteCache::new();
        cache.lease("BTC", NOW);
        let ttl = LEASE_TTL.as_millis() as u64;

        assert_eq!(cache.leased(NOW + ttl - 1), ["BTC"], "still inside the ttl");
        assert!(cache.leased(NOW + ttl).is_empty(), "and out the other side");
    }

    /// Renewing on read is what makes the TTL mean "since anybody last cared".
    #[test]
    fn asking_again_renews_the_lease() {
        let cache = QuoteCache::new();
        let ttl = LEASE_TTL.as_millis() as u64;
        cache.lease("BTC", NOW);
        cache.lease("BTC", NOW + ttl - 1);
        assert_eq!(
            cache.leased(NOW + ttl + 1),
            ["BTC"],
            "the second ask moved the clock"
        );
    }

    /// The quote outlives the lease: it costs nothing to keep and answers a
    /// later look with something real, while the socket still goes back.
    #[test]
    fn an_expired_lease_drops_the_socket_and_keeps_the_last_quote() {
        let cache = QuoteCache::new();
        cache.lease("BTC", NOW);
        cache.observe(quote("BTC"));
        let _ = cache.leased(NOW + LEASE_TTL.as_millis() as u64);

        assert!(cache.leased(NOW).is_empty(), "no longer leased");
        assert!(
            cache.peek("BTC").is_some(),
            "but the last frame is still here"
        );
    }

    #[test]
    fn a_held_quote_comes_back_with_the_lease() {
        let cache = QuoteCache::new();
        cache.observe(quote("ETH"));
        assert_eq!(cache.lease("ETH", NOW).map(|q| q.coin), Some("ETH".into()));
    }

    /// A quiet symbol may not print at all, and the caller must not hang on
    /// it. Expiry is an absent tilt, never an error.
    #[tokio::test(start_paused = true)]
    async fn warming_a_silent_symbol_gives_up_rather_than_hanging() {
        let cache = QuoteCache::new();
        cache.lease("QUIET", NOW);
        assert_eq!(cache.warm("QUIET").await, None);
    }

    /// And a frame arriving mid-wait wakes the caller rather than being
    /// missed — the lost-wakeup the notify registration order exists to avoid.
    #[tokio::test(start_paused = true)]
    async fn a_frame_arriving_mid_wait_wakes_the_caller() {
        let cache = QuoteCache::new();
        cache.lease("BTC", NOW);
        let waiting = async { cache.warm("BTC").await };
        let arriving = async {
            tokio::time::sleep(WARMUP_WAIT / 3).await;
            cache.observe(quote("BTC"));
        };
        let (warmed, ()) = tokio::join!(waiting, arriving);
        assert!(warmed.is_some(), "the frame landed inside the budget");
    }

    #[test]
    fn a_sigma_is_reused_inside_its_ttl_and_refetched_after() {
        let cache = SigmaCache::new();
        let ttl = SIGMA_TTL.as_millis() as u64;
        let reading = Volatility {
            sigma_day: Decimal::ONE,
            vol_ratio: Some(Decimal::TWO),
        };
        cache.put("BTC", reading, NOW);

        assert_eq!(cache.get("BTC", NOW + ttl - 1), Some(reading));
        assert_eq!(
            cache.get("BTC", NOW + ttl),
            None,
            "past the ttl the caller measures again"
        );
    }

    /// The two halves come from one measurement, so a cache that let them
    /// expire apart could hand the guardrail an hour's ratio against a
    /// different day's sigma.
    #[test]
    fn the_hour_and_the_day_expire_together() {
        let cache = SigmaCache::new();
        let ttl = SIGMA_TTL.as_millis() as u64;
        cache.put(
            "BTC",
            Volatility {
                sigma_day: Decimal::ONE,
                vol_ratio: Some(Decimal::TWO),
            },
            NOW,
        );

        assert_eq!(cache.get("BTC", NOW + ttl), None);
    }

    #[test]
    fn a_symbol_never_measured_has_no_sigma() {
        assert_eq!(SigmaCache::new().get("BTC", NOW), None);
    }
}
