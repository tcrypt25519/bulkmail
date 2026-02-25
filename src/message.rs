//! Transaction intent. See [`Message`].
//!
//! [`Message`]: Message

use alloy::primitives::{Address, Bytes, U256};
use std::fmt::Display;

pub(crate) const MAX_PRIORITY: u32 = 100;

/// Block time used to convert elapsed milliseconds into an age-factor score.
///
/// Matches the 2-second target block time of most fast EVM chains. Change
/// this if deploying against a chain with a different block time.
pub(crate) const BLOCK_TIME_MS: u64 = 2_000;

const POINTS_PER_BLOCK: u32 = 1;
const MAX_RETRIES: u32 = 3;

/// Number of blocks remaining at which the deadline boost reaches its maximum.
///
/// Below this threshold the message receives the full [`MAX_PRIORITY`] boost.
const DEADLINE_BOOST_NEAR_BLOCKS: u64 = 2;

/// Number of blocks remaining at which the deadline boost begins.
///
/// Beyond this threshold there is no boost. Between [`DEADLINE_BOOST_NEAR_BLOCKS`]
/// and this value the boost increases linearly from `MAX_PRIORITY / 3` to
/// `MAX_PRIORITY` as the deadline approaches.
const DEADLINE_BOOST_FAR_BLOCKS: u64 = 10;

/// A transaction intent - the data needed to build and send an EIP-1559 transaction.
///
/// [`Message`] separates intent from execution. Nonces and gas prices are not
/// stored here; they are bound late by the nonce and gas-price managers
/// immediately before submission. This allows messages to be re-queued and
/// retried without invalidating earlier state.
///
/// ## Time representation
///
/// `created_at_ms` is an absolute Unix epoch timestamp in milliseconds.
/// `deadline_ms` is a **relative** duration in milliseconds from `created_at_ms`,
/// so the absolute expiry is `created_at_ms + deadline_ms as u64`. Using `u32`
/// caps the maximum deadline at roughly 49.7 days, which is well above any
/// practical transaction timeout.
///
/// ## Priority
///
/// [`effective_priority`] combines `priority`, `retry_count`, elapsed age in
/// blocks, and a deadline boost. Messages approaching their deadline within
/// [`DEADLINE_BOOST_NEAR_BLOCKS`] blocks receive the full `MAX_PRIORITY`
/// boost; within [`DEADLINE_BOOST_FAR_BLOCKS`] blocks the boost increases
/// linearly. A message whose deadline has already passed returns an effective
/// priority of 0, which causes it to be filtered out before sending.
///
/// [`effective_priority`]: Self::effective_priority
#[derive(Debug, Clone)]
pub struct Message {
    /// The recipient address, or `None` for a contract-creation transaction.
    pub to: Option<Address>,

    /// The ETH value to transfer, in wei.
    pub value: U256,

    /// The call data payload.
    pub data: Bytes,

    /// The gas limit for this transaction.
    pub gas: u64,

    /// The caller-assigned base priority (0 to 100).
    pub priority: u32,

    /// Optional deadline as milliseconds relative to `created_at_ms`.
    ///
    /// The maximum representable deadline is `u32::MAX` ms ≈ 49.7 days.
    /// Pass `None` for messages that should never expire.
    pub deadline_ms: Option<u32>,

    /// Unix epoch timestamp in milliseconds at which this message was created.
    ///
    /// Used to compute the age factor in [`effective_priority`] and to
    /// resolve the absolute expiry time from the relative [`deadline_ms`].
    ///
    /// [`effective_priority`]: Self::effective_priority
    /// [`deadline_ms`]: Self::deadline_ms
    pub(crate) created_at_ms: u64,

    /// The number of times this message has been retried after a dropped transaction.
    retry_count: u32,
}

impl Message {
    /// Creates a new [`Message`] with the given parameters and zero retries.
    ///
    /// `now_ms` must be the current Unix epoch time in milliseconds (e.g.
    /// from [`SystemClock::now_ms`]). It is stored as `created_at_ms` and
    /// used later to compute age and deadline factors.
    ///
    /// [`SystemClock::now_ms`]: crate::clock::SystemClock::now_ms
    pub fn new(
        to: Option<Address>,
        gas: u64,
        value: U256,
        data: Bytes,
        priority: u32,
        deadline_ms: Option<u32>,
        now_ms: u64,
    ) -> Self {
        Self {
            to,
            gas,
            value,
            data,
            priority: priority.min(MAX_PRIORITY),
            deadline_ms,
            created_at_ms: now_ms,
            retry_count: 0,
        }
    }

    /// Returns the computed priority used to order this message in the queue.
    ///
    /// The formula is:
    ///
    /// ```text
    /// effective_priority = priority + retry_count + age_factor + deadline_factor
    /// ```
    ///
    /// Returns `0` if the message deadline has already passed, which causes
    /// [`is_expired`] to filter it before sending.
    ///
    /// `now_ms` must be the current Unix epoch time in milliseconds.
    ///
    /// [`is_expired`]: Self::is_expired
    pub fn effective_priority(&self, now_ms: u64) -> u32 {
        let deadline_factor = match self.deadline_factor(now_ms) {
            None => 0,
            Some(0) => return 0,
            Some(x) => x,
        };

        let age_ms = now_ms.saturating_sub(self.created_at_ms);
        let age_factor = (age_ms / BLOCK_TIME_MS) as u32 * POINTS_PER_BLOCK;

        self.priority + self.retry_count + age_factor + deadline_factor
    }

    /// Returns a boost factor derived from the time remaining until the deadline.
    ///
    /// - `None` — no deadline set, or more than [`DEADLINE_BOOST_FAR_BLOCKS`] remain.
    /// - `Some(0)` — the deadline has already passed; the message should be
    ///   discarded (see [`effective_priority`]).
    /// - `Some(n)` — linear interpolation between `MAX_PRIORITY / 3` (at the
    ///   far threshold) and `MAX_PRIORITY` (at the near threshold).
    ///
    /// [`effective_priority`]: Self::effective_priority
    fn deadline_factor(&self, now_ms: u64) -> Option<u32> {
        let deadline_ms = self.deadline_ms?;
        let abs_deadline_ms = self.created_at_ms + deadline_ms as u64;

        if now_ms >= abs_deadline_ms {
            return Some(0);
        }

        let remaining_ms = abs_deadline_ms - now_ms;
        let remaining_blocks = remaining_ms / BLOCK_TIME_MS;

        if remaining_blocks < DEADLINE_BOOST_NEAR_BLOCKS {
            Some(MAX_PRIORITY)
        } else if remaining_blocks < DEADLINE_BOOST_FAR_BLOCKS {
            // Linear boost: MAX_PRIORITY / 3 at the far boundary, MAX_PRIORITY
            // at the near boundary, increasing as the deadline approaches.
            let range = DEADLINE_BOOST_FAR_BLOCKS - DEADLINE_BOOST_NEAR_BLOCKS;
            let dist_from_near = remaining_blocks - DEADLINE_BOOST_NEAR_BLOCKS;
            let max_boost = MAX_PRIORITY as u64;
            let min_boost = max_boost / 3;
            let boost = max_boost - (max_boost - min_boost) * dist_from_near / range;
            Some(boost as u32)
        } else {
            None
        }
    }

    /// Increments the retry counter and returns `true` if further retries are allowed.
    pub fn increment_retry(&mut self) -> bool {
        self.retry_count = self.retry_count.saturating_add(1);
        self.can_retry()
    }

    /// Returns `true` if this message has not yet exhausted its retry budget.
    pub fn can_retry(&self) -> bool {
        self.retry_count < MAX_RETRIES
    }

    /// Returns `true` if the message deadline has passed and the message should be discarded.
    ///
    /// `now_ms` must be the current Unix epoch time in milliseconds.
    pub fn is_expired(&self, now_ms: u64) -> bool {
        match self.deadline_ms {
            None => false,
            Some(d) => now_ms >= self.created_at_ms + d as u64,
        }
    }
}

impl Default for Message {
    fn default() -> Self {
        use crate::clock::{Clock, SystemClock};
        Self {
            to: None,
            value: Default::default(),
            data: Default::default(),
            gas: 0,
            priority: 0,
            deadline_ms: None,
            created_at_ms: SystemClock.now_ms(),
            retry_count: 0,
        }
    }
}

impl Display for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Message {{ to: {:?}, value: {}, gas: {}, priority: {}, \
             deadline_ms: {:?}, created_at_ms: {}, retry_count: {} }}",
            self.to,
            self.value,
            self.gas,
            self.priority,
            self.deadline_ms,
            self.created_at_ms,
            self.retry_count
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::test_support::ManualClock;
    use alloy::primitives::Address;

    // ── Message::new guarantees ──────────────────────────────────────────────

    #[test]
    fn test_new_clamps_priority_to_max() {
        let msg = Message::new(None, 21_000, U256::ZERO, Default::default(), 200, None, 0);
        assert_eq!(msg.priority, MAX_PRIORITY);
    }

    #[test]
    fn test_new_accepts_priority_at_max() {
        let msg = Message::new(None, 21_000, U256::ZERO, Default::default(), MAX_PRIORITY, None, 0);
        assert_eq!(msg.priority, MAX_PRIORITY);
    }

    #[test]
    fn test_new_stores_now_ms() {
        let msg = Message::new(None, 0, U256::ZERO, Default::default(), 0, None, 12_345);
        assert_eq!(msg.created_at_ms, 12_345);
    }

    // ── effective_priority ───────────────────────────────────────────────────

    #[test]
    fn test_effective_priority_base() {
        let msg = Message::new(None, 0, U256::ZERO, Default::default(), 5, None, 0);
        assert_eq!(msg.effective_priority(0), 5);
    }

    #[test]
    fn test_effective_priority_includes_retry_count() {
        let mut msg = Message::new(None, 0, U256::ZERO, Default::default(), 1, None, 0);
        msg.increment_retry();
        assert_eq!(msg.effective_priority(0), 2);
    }

    #[test]
    fn test_effective_priority_age_factor() {
        // Created at 0; now is 4_000 ms (2 blocks). Each block gives POINTS_PER_BLOCK = 1.
        let msg = Message::new(None, 0, U256::ZERO, Default::default(), 0, None, 0);
        assert_eq!(msg.effective_priority(4_000), 2);
    }

    #[test]
    fn test_effective_priority_zero_when_deadline_passed() {
        // deadline_ms = 1_000 ms from creation (0 ms). At now=1_000 it has expired.
        let msg = Message::new(None, 0, U256::ZERO, Default::default(), 50, Some(1_000), 0);
        assert_eq!(msg.effective_priority(1_000), 0);
        assert_eq!(msg.effective_priority(2_000), 0);
    }

    // ── deadline_factor / priority boost ────────────────────────────────────

    #[test]
    fn test_deadline_factor_none_when_no_deadline() {
        let msg = Message::new(None, 0, U256::ZERO, Default::default(), 0, None, 0);
        assert_eq!(msg.deadline_factor(0), None);
    }

    #[test]
    fn test_deadline_factor_none_when_far_away() {
        // 11 blocks remaining > DEADLINE_BOOST_FAR_BLOCKS (10).
        let far_ms = DEADLINE_BOOST_FAR_BLOCKS * BLOCK_TIME_MS + BLOCK_TIME_MS; // 11 blocks
        let msg = Message::new(None, 0, U256::ZERO, Default::default(), 0, Some(far_ms as u32), 0);
        assert_eq!(msg.deadline_factor(0), None);
    }

    #[test]
    fn test_deadline_factor_max_when_near() {
        // 1 block remaining < DEADLINE_BOOST_NEAR_BLOCKS (2).
        let near_ms = DEADLINE_BOOST_NEAR_BLOCKS * BLOCK_TIME_MS - 1; // just under 2 blocks
        let msg = Message::new(None, 0, U256::ZERO, Default::default(), 0, Some(near_ms as u32), 0);
        assert_eq!(msg.deadline_factor(0), Some(MAX_PRIORITY));
    }

    #[test]
    fn test_deadline_factor_linear_between_thresholds() {
        // Midpoint: 6 blocks remaining (between NEAR=2 and FAR=10).
        // dist_from_near = 6 - 2 = 4, range = 10 - 2 = 8.
        // boost = 100 - (100 - 33) * 4 / 8 = 100 - 67*4/8 = 100 - 33 = 67.
        let mid_ms = 6 * BLOCK_TIME_MS;
        let msg = Message::new(None, 0, U256::ZERO, Default::default(), 0, Some(mid_ms as u32), 0);
        let factor = msg.deadline_factor(0).unwrap();
        assert!(factor > MAX_PRIORITY / 3, "boost should be above the minimum");
        assert!(factor < MAX_PRIORITY, "boost should be below the maximum");
    }

    #[test]
    fn test_deadline_factor_increases_as_deadline_approaches() {
        // At 9 blocks remaining, boost should be lower than at 3 blocks remaining.
        let msg9 = Message::new(None, 0, U256::ZERO, Default::default(), 0, Some((9 * BLOCK_TIME_MS) as u32), 0);
        let msg3 = Message::new(None, 0, U256::ZERO, Default::default(), 0, Some((3 * BLOCK_TIME_MS) as u32), 0);
        let boost9 = msg9.deadline_factor(0).unwrap();
        let boost3 = msg3.deadline_factor(0).unwrap();
        assert!(boost3 > boost9, "closer deadline must yield a higher boost");
    }

    // ── is_expired ───────────────────────────────────────────────────────────

    #[test]
    fn test_is_expired_with_past_deadline() {
        // deadline_ms = 500; created at 0. At now=500 it is exactly expired.
        let msg = Message::new(None, 0, U256::ZERO, Default::default(), 0, Some(500), 0);
        assert!(msg.is_expired(500));
        assert!(msg.is_expired(1_000));
    }

    #[test]
    fn test_is_not_expired_with_no_deadline() {
        let msg = Message::new(None, 0, U256::ZERO, Default::default(), 0, None, 0);
        assert!(!msg.is_expired(u64::MAX));
    }

    #[test]
    fn test_is_not_expired_before_deadline() {
        let msg = Message::new(None, 0, U256::ZERO, Default::default(), 0, Some(1_000), 0);
        assert!(!msg.is_expired(999));
    }

    // ── ManualClock deadline tests ───────────────────────────────────────────

    #[test]
    fn test_deadline_expiry_with_manual_clock() {
        use crate::clock::Clock;
        let clock = ManualClock::new(1_000_000);
        let deadline_ms: u32 = 5_000; // 5 seconds
        let msg = Message::new(None, 0, U256::ZERO, Default::default(), 0, Some(deadline_ms), clock.now_ms());

        // Not yet expired
        assert!(!msg.is_expired(clock.now_ms()));

        // Advance past the deadline
        clock.advance(deadline_ms as u64);
        assert!(msg.is_expired(clock.now_ms()));
    }

    #[test]
    fn test_priority_boost_activates_with_manual_clock() {
        use crate::clock::Clock;
        let clock = ManualClock::new(0);

        // 20 blocks of deadline from t=0
        let deadline_ms = (20 * BLOCK_TIME_MS) as u32;
        let msg = Message::new(
            None,
            0,
            U256::ZERO,
            Default::default(),
            0,
            Some(deadline_ms),
            clock.now_ms(),
        );

        // At t=0, 20 blocks remain → no boost
        assert_eq!(msg.deadline_factor(clock.now_ms()), None);

        // Advance to leave only 5 blocks remaining → linear boost zone
        clock.advance((15 * BLOCK_TIME_MS) as u64);
        let factor = msg.deadline_factor(clock.now_ms());
        assert!(factor.is_some(), "expected a boost at 5 blocks remaining");
        assert!(factor.unwrap() > MAX_PRIORITY / 3);
        assert!(factor.unwrap() < MAX_PRIORITY);

        // Advance to leave only 1 block remaining → maximum boost
        clock.advance((4 * BLOCK_TIME_MS) as u64);
        assert_eq!(msg.deadline_factor(clock.now_ms()), Some(MAX_PRIORITY));
    }

    // ── can_retry / increment_retry ──────────────────────────────────────────

    #[test]
    fn test_can_retry_exhausted_after_max_retries() {
        let mut msg = Message::new(None, 0, U256::ZERO, Default::default(), 0, None, 0);
        assert!(msg.can_retry());
        for _ in 0..MAX_RETRIES {
            msg.increment_retry();
        }
        assert!(!msg.can_retry());
    }

    #[test]
    fn test_increment_retry_returns_true_while_budget_remains() {
        let mut msg = Message::new(None, 0, U256::ZERO, Default::default(), 0, None, 0);
        for _ in 0..(MAX_RETRIES - 1) {
            assert!(msg.increment_retry(), "should still have retries left");
        }
        assert!(!msg.increment_retry(), "budget exhausted on final call");
    }

    // ── Display ──────────────────────────────────────────────────────────────

    #[test]
    fn test_display_format() {
        let s = format!("{}", Message::default());
        assert!(s.starts_with("Message {"), "unexpected Display output: {s}");
        assert!(s.contains("retry_count: 0"));
    }

    // ── Table tests ──────────────────────────────────────────────────────────

    #[test]
    fn test_effective_priority_table() {
        struct Case {
            priority: u32,
            retries: u32,
            age_ms: u64,
            expected: u32,
        }
        let cases = [
            Case { priority: 0, retries: 0, age_ms: 0, expected: 0 },
            Case { priority: 5, retries: 0, age_ms: 0, expected: 5 },
            Case { priority: 5, retries: 2, age_ms: 0, expected: 7 },
            // 2 blocks of age = 4_000 ms → age_factor = 2
            Case { priority: 5, retries: 0, age_ms: 4_000, expected: 7 },
            Case { priority: 5, retries: 1, age_ms: 4_000, expected: 8 },
        ];
        for c in &cases {
            let mut msg = Message::new(None, 0, U256::ZERO, Default::default(), c.priority, None, 0);
            for _ in 0..c.retries {
                msg.increment_retry();
            }
            assert_eq!(
                msg.effective_priority(c.age_ms),
                c.expected,
                "priority={} retries={} age_ms={}",
                c.priority, c.retries, c.age_ms
            );
        }
    }

    #[test]
    fn test_is_expired_table() {
        struct Case {
            created_at_ms: u64,
            deadline_ms: Option<u32>,
            now_ms: u64,
            expected: bool,
        }
        let cases = [
            Case { created_at_ms: 0, deadline_ms: None,       now_ms: 9999,  expected: false },
            Case { created_at_ms: 0, deadline_ms: Some(1000), now_ms: 999,   expected: false },
            Case { created_at_ms: 0, deadline_ms: Some(1000), now_ms: 1000,  expected: true  },
            Case { created_at_ms: 0, deadline_ms: Some(1000), now_ms: 2000,  expected: true  },
            Case { created_at_ms: 5000, deadline_ms: Some(3000), now_ms: 7999, expected: false },
            Case { created_at_ms: 5000, deadline_ms: Some(3000), now_ms: 8000, expected: true  },
        ];
        for c in &cases {
            let msg = Message::new(None, 0, U256::ZERO, Default::default(), 0, c.deadline_ms, c.created_at_ms);
            assert_eq!(
                msg.is_expired(c.now_ms),
                c.expected,
                "created_at={} deadline={:?} now={}",
                c.created_at_ms, c.deadline_ms, c.now_ms
            );
        }
    }

    // ── Message::new with to field ───────────────────────────────────────────

    #[test]
    fn test_new_message_fields() {
        let addr = Some(Address::default());
        let msg = Message::new(addr, 21_000, U256::ZERO, Default::default(), 1, None, 42);
        assert_eq!(msg.priority, 1);
        assert_eq!(msg.retry_count, 0);
        assert_eq!(msg.created_at_ms, 42);
        assert_eq!(msg.gas, 21_000);
    }
}
