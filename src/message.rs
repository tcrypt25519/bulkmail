//! Transaction intent. See [`Message`].
//!
//! [`Message`]: Message

use alloy::primitives::{Address, Bytes, U256};
use chrono::{DateTime, Utc};
use std::{fmt::Display, time::Instant};

pub(crate) const MAX_PRIORITY: u32 = 100;
const BLOCK_TIME: u32 = 2;
const POINTS_PER_BLOCK: u32 = 1;
const MAX_RETRIES: u32 = 3;

/// A transaction intent - the data needed to build and send an EIP-1559 transaction.
///
/// [`Message`] separates intent from execution. Nonces and gas prices are not
/// stored here; they are bound late by the nonce and gas-price managers
/// immediately before submission. This allows messages to be re-queued and
/// retried without invalidating earlier state.
///
/// ## Priority
///
/// [`effective_priority`] combines `priority`, `retry_count`, elapsed age in
/// blocks, and a deadline boost. Messages approaching their deadline within 2
/// blocks receive a full priority boost (100 points); within 10 blocks,
/// one-third of that. A message whose deadline has already passed returns an
/// effective priority of 0, causing it to be filtered out before sending.
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

    /// An optional block deadline after which this message should be discarded.
    pub deadline: Option<DateTime<Utc>>,

    /// The wall-clock time at which this message was created, used to compute
    /// the age factor in [`effective_priority`].
    ///
    /// [`effective_priority`]: Self::effective_priority
    created_at: Instant,

    /// The number of times this message has been retried after a dropped transaction.
    retry_count: u32,
}

impl Message {
    /// Creates a new [`Message`] with the given parameters and zero retries.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        to: Option<Address>,
        gas: u64,
        value: U256,
        data: Bytes,
        priority: u32,
        deadline: Option<DateTime<Utc>>,
    ) -> Self {
        Self {
            to,
            gas,
            value,
            data,
            priority: priority.min(MAX_PRIORITY),
            deadline,
            ..Default::default()
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
    /// [`is_expired`]: Self::is_expired
    pub fn effective_priority(&self) -> u32 {
        // Get the deadline factor.
        // If it's None then we don't influence the priority.
        // If it's Some(0) then the deadline has passed and we set the priority to 0.
        // Otherwise, we add the factor to the priority.
        let deadline_factor = match self.deadline_factor() {
            None => 0,
            Some(0) => return 0,
            Some(x) => x,
        };

        let age = self.created_at.elapsed().as_secs() as u32;
        let age_factor = age * POINTS_PER_BLOCK / BLOCK_TIME;

        self.priority + self.retry_count + age_factor + deadline_factor
    }

    /// Returns a factor to boost the priority based on the deadline.
    ///
    /// Returns `None` if there is no deadline or the deadline is more than 10
    /// blocks away. Returns `Some(0)` if the deadline has passed.
    fn deadline_factor(&self) -> Option<u32> {
        match self.deadline {
            None => None,
            Some(deadline) => {
                let now = Utc::now();
                if deadline <= now {
                    return Some(0);
                }

                let time_left = deadline - now;
                let seconds_left = time_left.num_seconds() as u32;
                if seconds_left < (BLOCK_TIME * 2) {
                    // Less than 2 blocks left, maximum priority boost
                    Some(MAX_PRIORITY)
                } else if seconds_left < (BLOCK_TIME * 10) {
                    // Less than 10 blocks left, medium priority boost
                    Some(MAX_PRIORITY / 3)
                } else {
                    // More than 10 blocks left, no priority boost
                    None
                }
            }
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
    pub fn is_expired(&self) -> bool {
        if let Some(deadline) = self.deadline {
            Utc::now() > deadline
        } else {
            false
        }
    }
}

impl Default for Message {
    fn default() -> Self {
        Self {
            to: None,
            value: Default::default(),
            data: Default::default(),
            gas: 0,
            priority: 0,
            deadline: None,
            created_at: Instant::now(),
            retry_count: 0,
        }
    }
}

impl Display for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Message {{ to: {:?}, value: {}, gas: {}, priority: {}, deadline: {:?}, created_at: {:?}, retry_count: {} }}",
            self.to,
            self.value,
            self.gas,
            self.priority,
            self.deadline,
            self.created_at,
            self.retry_count
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_message() {
        let to = Some(Address::default());
        let gas = 21_000u64;
        let value = U256::from(0);
        let data = Bytes::default();
        let priority = 1;
        let deadline = None;
        let message = Message::new(to, gas, value, data, priority, deadline);

        assert_eq!(message.priority, 1);
        assert_eq!(message.retry_count, 0);
    }

    #[test]
    fn test_effective_priority() {
        let to = Some(Address::default());
        let gas = 21_000u64;
        let value = U256::from(0);
        let data = Bytes::default();
        let priority = 1;
        let deadline = None;
        let mut message = Message::new(to, gas, value, data, priority, deadline);

        assert_eq!(message.effective_priority(), 1);

        message.increment_retry();
        assert_eq!(message.effective_priority(), 2);

        // Simulate passage of time
        // TODO: Add a way to mock time
        // std::thread::sleep(Duration::from_secs(13));
        // assert_eq!(message.effective_priority(), 3);
    }

    #[test]
    fn test_can_retry() {
        let to = Some(Address::default());
        let gas = 21_000u64;
        let value = U256::from(0);
        let data = Bytes::default();
        let priority = 1;
        let deadline = None;
        let mut message = Message::new(to, gas, value, data, priority, deadline);

        assert!(message.can_retry());

        for _ in 0..MAX_RETRIES {
            message.increment_retry();
        }

        assert!(!message.can_retry());
    }

    // ── is_expired ───────────────────────────────────────────────────────────

    #[test]
    fn test_is_expired_with_past_deadline() {
        let mut msg = Message::default();
        let past: DateTime<Utc> = "2000-01-01T00:00:00Z".parse().unwrap();
        msg.deadline = Some(past);
        assert!(msg.is_expired());
    }

    #[test]
    fn test_is_not_expired_with_no_deadline() {
        assert!(!Message::default().is_expired());
    }

    #[test]
    fn test_is_not_expired_with_future_deadline() {
        let mut msg = Message::default();
        let future: DateTime<Utc> = "2099-01-01T00:00:00Z".parse().unwrap();
        msg.deadline = Some(future);
        assert!(!msg.is_expired());
    }

    // ── effective_priority edge cases ────────────────────────────────────────

    #[test]
    fn test_effective_priority_zero_when_deadline_passed() {
        let mut msg = Message::default();
        msg.priority = 50;
        let past: DateTime<Utc> = "2000-01-01T00:00:00Z".parse().unwrap();
        msg.deadline = Some(past);
        assert_eq!(msg.effective_priority(), 0);
    }

    // ── Message::new guarantees ──────────────────────────────────────────────

    #[test]
    fn test_new_clamps_priority_to_max() {
        // Priorities above MAX_PRIORITY must be silently clamped at construction.
        let msg = Message::new(None, 21_000, U256::ZERO, Bytes::default(), 200, None);
        assert_eq!(msg.priority, MAX_PRIORITY);
    }

    #[test]
    fn test_new_accepts_priority_at_max() {
        let msg = Message::new(
            None,
            21_000,
            U256::ZERO,
            Bytes::default(),
            MAX_PRIORITY,
            None,
        );
        assert_eq!(msg.priority, MAX_PRIORITY);
    }

    // ── Display ──────────────────────────────────────────────────────────────

    #[test]
    fn test_display_format() {
        let s = format!("{}", Message::default());
        assert!(s.starts_with("Message {"), "unexpected Display output: {s}");
        assert!(s.contains("retry_count: 0"));
    }
}
