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
pub(super) enum BucketError {
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
pub(super) struct TokenBucket {
    rate: OrderRate,
    capacity_micro: u64,
    tokens_micro: u64,
    last_ms: u64,
    /// Numerator left over from the last refill, in the same units as
    /// `elapsed × capacity_micro`, always strictly below `rate.per_ms`. This
    /// is what makes the refill exact: see [`TokenBucket::refill`].
    carry: u64,
}

impl TokenBucket {
    pub(super) fn new(rate: OrderRate, now_ms: u64) -> Self {
        let capacity_micro = u64::from(rate.count).saturating_mul(MICRO);
        TokenBucket {
            rate,
            capacity_micro,
            tokens_micro: capacity_micro,
            last_ms: now_ms,
            carry: 0,
        }
    }

    pub(super) fn rate(&self) -> OrderRate {
        self.rate
    }

    /// Tokens currently available, for `get_state`'s guardrail-utilization
    /// block (spec item 16). Does not refill; call [`TokenBucket::refill`]
    /// first if the caller wants the value as of now.
    pub(super) fn tokens(&self) -> Decimal {
        Decimal::from(self.tokens_micro) / Decimal::from(MICRO)
    }

    /// Accrues tokens for the elapsed interval.
    ///
    /// The clock always advances to `now_ms` and the sub-token remainder is
    /// kept as an exact **numerator** rather than as un-advanced time. That
    /// distinction is the whole correctness of this type. Converting the
    /// granted tokens back into milliseconds and rewinding the clock by that
    /// much floors twice — once granting the tokens, once charging for them —
    /// and the residual milliseconds are then charged again on the next call.
    /// At D-c's 5-per-300 s that leaks roughly a factor of two: a caller
    /// polling every millisecond used to see its first refilled token at
    /// 30,304 ms instead of 60,000 ms and fit 14 orders into a 300 s window
    /// against a cap of 10.
    ///
    /// With the carry, `tokens(t)` depends only on `t`, never on how often it
    /// was asked: `carry + Σ(elapsedᵢ × capacity)` telescopes to
    /// `total_elapsed × capacity`, so polling accrues exactly what one call
    /// at the same instant would.
    pub(super) fn refill(&mut self, now_ms: u64) -> Result<(), BucketError> {
        if self.rate.per_ms == 0 {
            return Err(BucketError::InvalidRate);
        }
        if now_ms < self.last_ms {
            return Err(BucketError::ClockWentBackwards {
                last_ms: self.last_ms,
            });
        }
        let elapsed = u128::from(now_ms - self.last_ms);
        // `elapsed ≤ u64::MAX` and `capacity_micro ≤ u32::MAX × 10⁶`, so the
        // product is below 8e34 and cannot overflow a u128.
        let numerator =
            elapsed.saturating_mul(u128::from(self.capacity_micro)) + u128::from(self.carry);
        let per = u128::from(self.rate.per_ms);
        let gain = numerator / per;
        // Strictly below `per_ms`, so it always fits a u64.
        self.carry = u64::try_from(numerator % per).unwrap_or(0);
        self.last_ms = now_ms;
        if gain > 0 {
            let gained = u64::try_from(gain).unwrap_or(u64::MAX);
            self.tokens_micro = self.tokens_micro.saturating_add(gained);
            if self.tokens_micro >= self.capacity_micro {
                self.tokens_micro = self.capacity_micro;
                // A full bucket has nothing to carry: keeping the remainder
                // would hand out a free fraction of a token the moment one is
                // spent.
                self.carry = 0;
            }
        }
        Ok(())
    }

    /// Refills, then spends one token. On failure nothing is spent, so a
    /// refused order never costs an agent budget it did not use.
    pub(super) fn try_take(&mut self, now_ms: u64) -> Result<(), BucketError> {
        self.refill(now_ms)?;
        self.peek()?;
        self.tokens_micro -= MICRO;
        Ok(())
    }

    /// The answer [`TokenBucket::try_take`] would give, without taking.
    ///
    /// Spec item 20's preflight has to report the order-rate cap it would hit
    /// while costing nothing — and it must report it in the *same* terms, so
    /// the two are one expression rather than two that can drift. Assumes the
    /// caller has already refilled.
    pub(super) fn peek(&self) -> Result<(), BucketError> {
        if self.tokens_micro < MICRO {
            return Err(BucketError::Empty {
                tokens_available_micro: self.tokens_micro,
                retry_after_ms: self.retry_after_ms(),
            });
        }
        Ok(())
    }

    /// Milliseconds until one whole token exists, rounded up. `u64::MAX`
    /// when the capacity is zero, because no wait ever produces a token.
    pub(super) fn retry_after_ms(&self) -> u64 {
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

    /// The cap must bound orders, not calls. An agent that retries every
    /// millisecond has to end the window with exactly the same budget as one
    /// that waited and asked once, or the rate cap is defeated by polling.
    ///
    /// Before the carry fix this admitted 14 orders in the 300 s window and
    /// handed out its first refilled token at 30,304 ms.
    #[test]
    fn polling_every_millisecond_does_not_farm_extra_tokens() {
        let mut b = TokenBucket::new(rate(5, 300_000), 0);
        let mut admitted = 0;
        for _ in 0..5 {
            assert!(b.try_take(0).is_ok());
            admitted += 1;
        }
        let mut first_refill_ms = None;
        for ms in 1..=300_000 {
            if b.try_take(ms).is_ok() {
                admitted += 1;
                first_refill_ms.get_or_insert(ms);
            }
        }
        assert_eq!(
            first_refill_ms,
            Some(60_000),
            "one token per 60s at 5 per 300s, however often it is asked"
        );
        assert_eq!(
            admitted, 10,
            "5 initial + 5 refilled is the whole 300s budget"
        );
    }

    /// The same property stated directly: the token count at an instant is a
    /// function of the instant, not of the call pattern that reached it.
    #[test]
    fn accrual_is_independent_of_how_often_refill_is_called() {
        let rate = rate(5, 300_000);
        let mut polled = TokenBucket::new(rate, 0);
        let mut quiet = TokenBucket::new(rate, 0);
        for _ in 0..5 {
            assert!(polled.try_take(0).is_ok());
            assert!(quiet.try_take(0).is_ok());
        }
        for ms in 1..=59_999 {
            polled.refill(ms).expect("refill");
        }
        quiet.refill(59_999).expect("refill");
        assert_eq!(polled.tokens(), quiet.tokens());
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
