//! Max-heap priority queue for [`Message`] values. See [`PriorityQueue`].
//!
//! [`Message`]: crate::Message

use crate::message::Message;
use std::{cmp::Ordering, collections::BinaryHeap};

/// A max-heap queue that orders [`Message`] values by their [`effective_priority`].
///
/// [`effective_priority`] is evaluated at push time and stored alongside the
/// message. This means priority does not change while a message sits in the
/// queue - callers that need dynamic re-prioritization should pop and re-push
/// the message.
///
/// [`Message`]: crate::Message
/// [`effective_priority`]: crate::Message::effective_priority
#[derive(Debug)]
pub(crate) struct PriorityQueue {
    heap: BinaryHeap<PrioritizedMessage>,
}

#[derive(Debug)]
struct PrioritizedMessage {
    message: Message,
    effective_priority: u32,
}

impl Eq for PrioritizedMessage {}

impl PartialEq for PrioritizedMessage {
    fn eq(&self, other: &Self) -> bool {
        self.effective_priority == other.effective_priority
    }
}

impl Ord for PrioritizedMessage {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse ordering for max-heap behavior
        // other.effective_priority.cmp(&self.effective_priority)
        self.effective_priority.cmp(&other.effective_priority)
    }
}

impl PartialOrd for PrioritizedMessage {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PriorityQueue {
    /// Creates a new empty [`PriorityQueue`].
    pub fn new() -> Self {
        Self {
            heap: BinaryHeap::new(),
        }
    }

    /// Pushes `message` onto the queue, capturing its effective priority at
    /// insertion time.
    pub fn push(&mut self, message: Message) {
        let effective_priority = message.effective_priority();
        self.heap.push(PrioritizedMessage {
            message,
            effective_priority,
        });
    }

    /// Removes and returns the highest-priority message, or `None` if empty.
    pub fn pop(&mut self) -> Option<Message> {
        self.heap.pop().map(|pm| pm.message)
    }

    /// Returns `true` if the queue contains no messages.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    /// Returns the number of messages currently in the queue.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.heap.len()
    }
}

impl Default for PriorityQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::Address;
    use super::*;
    use crate::{Error, Message};

    #[test]
    fn test_priority_queue() -> Result<(), Error> {
        let mut queue = PriorityQueue::new();

        let mut msg1 = Message::default();
        msg1.to = Some(Address::default());

        let mut msg2 = Message::default();
        msg2.to = Some(Address::default());
        msg2.priority = 1;

        let mut msg3 = Message::default();
        msg3.to = Some(Address::default());
        msg3.priority = 2;

        queue.push(msg1);
        queue.push(msg2);
        queue.push(msg3);

        assert_eq!(queue.len(), 3);
        assert!(!queue.is_empty());

        // Messages should come out in order of priority
        assert_eq!(queue.pop().unwrap().priority, 2);
        assert_eq!(queue.pop().unwrap().priority, 1);
        assert_eq!(queue.pop().unwrap().priority, 0);
        assert!(queue.is_empty());

        Ok(())
    }

    #[test]
    fn test_prioritized_message_ordering() -> Result<(), Error> {
        let mut msg1 = Message::default();
        msg1.priority = 1;

        let mut msg2 = Message::default();
        msg2.priority = 2;

        let pm1 = PrioritizedMessage {
            message: msg1,
            effective_priority: 1,
        };

        let pm2 = PrioritizedMessage {
            message: msg2,
            effective_priority: 2,
        };

        assert!(pm1 < pm2);
        assert_eq!(pm1.cmp(&pm2), Ordering::Less);
        Ok(())
    }
}
