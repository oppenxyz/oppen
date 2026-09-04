//! The order-rate token bucket (spec item 24).
//!
//! Integer arithmetic throughout, in millionths of a token, so the same
//! sequence of timestamps produces the same sequence of verdicts on any
//! machine — no floats, no rounding drift, no wall-clock reads inside the
//! computation. `docs/specs/fair-value.md` §6.5 asks for exactly this
//! property of the quant engine; the guardrail engine needs it for the same
//! reason, which is that a refusal has to be reproducible after the fact.
//!
//! Refill is continuous rather than per-window: `count` tokens accrue evenly
//! over `per_ms`. A fixed window would let an agent send `2 × count` orders
//! across a window boundary, which is exactly the burst the cap exists to
//! stop.

use rust_decimal::Decimal;

use super::config::OrderRate;

/// One token, in the bucket's internal millionths.
const MICRO: u64 = 1_000_000;

/// Why a token could not be taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BucketError {
    /// Not enough tokens yet. `retry_after_ms` is the wait until one full
    /// token has accrued, rounded up.
    Empty {
        tokens_available_micro: u64,
        retry_after_ms: u64,
    },
    /// The clock moved backwards. Refilling from a negative interval would
    /// either underflow or hand out free tokens, so it fails closed.
    ClockWentBackwards { last_ms: u64 },
    /// `per_ms` is zero, so the refill rate is undefined.
    InvalidRate,
}

/// A continuously-refilling bucket of order permits for one agent.
///
/// Constructed full: a freshly paired agent may use its whole budget
/// immediately, and it is the caps in [`super::AgentGuardrails`], not the
/// rate, that make the first order small.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenBucket {
    rate: OrderRate,
    capacity_micro: u64,
    tokens_micro: u64,
    last_ms: u64,
}

impl TokenBucket {
    pub fn new(rate: OrderRate, now_ms: u64) -> Self {
        let capacity_micro = u64::from(rate.count).saturating_mul(MICRO);
        TokenBucket {
            rate,
            capacity_micro,
            tokens_micro: capacity_micro,
            last_ms: now_ms,
        }
    }

    pub fn rate(&self) -> OrderRate {
        self.rate
    }

    /// Tokens currently available, for `get_state`'s guardrail-utilization
    /// block (spec item 16). Does not refill; call [`TokenBucket::refill`]
    /// first if the caller wants the value as of now.
    pub fn tokens(&self) -> Decimal {
        Decimal::from(self.tokens_micro) / Decimal::from(MICRO)
    }

    /// Accrues tokens for the elapsed interval.
    ///
    /// The clock is advanced only by the interval actually converted into
    /// tokens, so the sub-token remainder of a short interval is kept rather
    /// than discarded — otherwise a stream of fast calls would refill at
    /// zero.
    pub fn refill(&mut self, now_ms: u64) -> Result<(), BucketError> {
        if self.rate.per_ms == 0 {
            return Err(BucketError::InvalidRate);
        }
        if now_ms < self.last_ms {
            return Err(BucketError::ClockWentBackwards {
                last_ms: self.last_ms,
            });
        }
        let elapsed = u128::from(now_ms - self.last_ms);
        let gain =
            elapsed.saturating_mul(u128::from(self.capacity_micro)) / u128::from(self.rate.per_ms);
        if gain == 0 {
            return Ok(());
        }
        let gained = u64::try_from(gain).unwrap_or(u64::MAX);
        self.tokens_micro = self.tokens_micro.saturating_add(gained);
        if self.tokens_micro >= self.capacity_micro {
            self.tokens_micro = self.capacity_micro;
            self.last_ms = now_ms;
            return Ok(());
        }
        let consumed_ms = gain.saturating_mul(u128::from(self.rate.per_ms))
            / u128::from(self.capacity_micro).max(1);
        self.last_ms = self
            .last_ms
            .saturating_add(u64::try_from(consumed_ms).unwrap_or(0))
            .min(now_ms);
        Ok(())
    }

    /// Refills, then spends one token. On failure nothing is spent, so a
    /// refused order never costs an agent budget it did not use.
    pub fn try_take(&mut self, now_ms: u64) -> Result<(), BucketError> {
        self.refill(now_ms)?;
        if self.tokens_micro < MICRO {
            return Err(BucketError::Empty {
                tokens_available_micro: self.tokens_micro,
                retry_after_ms: self.retry_after_ms(),
            });
        }
        self.tokens_micro -= MICRO;
        Ok(())
    }

    /// Milliseconds until one whole token exists, rounded up. `u64::MAX`
    /// when the capacity is zero, because no wait ever produces a token.
    fn retry_after_ms(&self) -> u64 {
        if self.capacity_micro == 0 {
            return u64::MAX;
        }
        let missing = u128::from(MICRO.saturating_sub(self.tokens_micro));
        let per = u128::from(self.rate.per_ms);
        let cap = u128::from(self.capacity_micro);
        let ms = missing.saturating_mul(per).div_ceil(cap);
        u64::try_from(ms).unwrap_or(u64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rate(count: u32, per_ms: u64) -> OrderRate {
        OrderRate { count, per_ms }
    }

    /// D-c's rate, exercised as an agent would: burn the budget, get told
    /// exactly how long to wait, wait that long, get exactly one token.
    #[test]
    fn refills_continuously_at_the_configured_rate() {
        let mut b = TokenBucket::new(rate(5, 300_000), 0);
        for i in 0..5 {
            assert!(b.try_take(0).is_ok(), "token {i} should be available");
        }
        let Err(BucketError::Empty { retry_after_ms, .. }) = b.try_take(0) else {
            panic!("the sixth order inside the window must be refused");
        };
        // One token per 60s at 5 per 300s.
        assert_eq!(retry_after_ms, 60_000);
        assert!(b.try_take(59_999).is_err());
        assert!(b.try_take(60_000).is_ok());
        assert!(b.try_take(60_000).is_err());
    }

    /// The remainder of a sub-token interval must survive, or a caller that
    /// polls every millisecond never accrues anything.
    #[test]
    fn sub_token_remainders_are_not_lost() {
        let mut b = TokenBucket::new(rate(1, 1_000), 0);
        assert!(b.try_take(0).is_ok());
        for ms in 1..1_000 {
            assert!(b.try_take(ms).is_err(), "token granted early at {ms}ms");
        }
        assert!(b.try_take(1_000).is_ok());
    }

    #[test]
    fn refill_saturates_at_capacity() {
        let mut b = TokenBucket::new(rate(5, 300_000), 0);
        for _ in 0..5 {
            assert!(b.try_take(0).is_ok());
        }
        b.refill(300_000 * 100).expect("refill");
        assert_eq!(b.tokens(), Decimal::from(5));
        for _ in 0..5 {
            assert!(b.try_take(300_000 * 100).is_ok());
        }
        assert!(b.try_take(300_000 * 100).is_err());
    }

    #[test]
    fn a_backwards_clock_fails_closed() {
        let mut b = TokenBucket::new(rate(5, 300_000), 10_000);
        assert_eq!(
            b.try_take(9_999),
            Err(BucketError::ClockWentBackwards { last_ms: 10_000 })
        );
    }

    #[test]
    fn a_zero_window_is_an_invalid_rate_not_an_infinite_one() {
        let mut b = TokenBucket::new(rate(5, 0), 0);
        assert_eq!(b.try_take(0), Err(BucketError::InvalidRate));
    }

    #[test]
    fn a_zero_count_never_grants_a_token() {
        let mut b = TokenBucket::new(rate(0, 300_000), 0);
        assert_eq!(
            b.try_take(u64::MAX / 2),
            Err(BucketError::Empty {
                tokens_available_micro: 0,
                retry_after_ms: u64::MAX,
            })
        );
    }

    #[test]
    fn a_refused_take_does_not_spend_a_token() {
        let mut b = TokenBucket::new(rate(1, 1_000), 0);
        assert!(b.try_take(0).is_ok());
        assert!(b.try_take(500).is_err());
        assert!(b.try_take(500).is_err());
        assert_eq!(b.tokens(), Decimal::from_parts(5, 0, 0, false, 1));
    }
}
