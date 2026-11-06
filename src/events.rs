//! Fan-out of incoming envelopes to `/api/v1/events` subscribers.
//!
//! When nobody is listening (the bot is restarting), envelopes wait in a bounded
//! backlog and go to the next subscriber, instead of vanishing.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;

pub type Event = Arc<str>;

pub struct Hub {
    tx: broadcast::Sender<Event>,
    backlog: Mutex<VecDeque<Event>>,
    capacity: usize,
}

impl Hub {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity.max(16));
        Self {
            tx,
            backlog: Mutex::new(VecDeque::new()),
            capacity,
        }
    }

    pub fn publish(&self, event: Event) {
        let mut backlog = self.backlog.lock().unwrap();
        if self.tx.receiver_count() == 0 {
            if backlog.len() == self.capacity {
                backlog.pop_front();
                tracing::warn!("event backlog full: dropped the oldest envelope");
            }
            backlog.push_back(event);
        } else {
            let _ = self.tx.send(event);
        }
    }

    /// Whatever was waiting, then the live feed. Taken under one lock so nothing
    /// published in between is lost or doubled.
    pub fn subscribe(&self) -> (Vec<Event>, broadcast::Receiver<Event>) {
        let mut backlog = self.backlog.lock().unwrap();
        let rx = self.tx.subscribe();
        (backlog.drain(..).collect(), rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holds_events_until_someone_listens() {
        let hub = Hub::new(2);
        hub.publish("a".into());
        hub.publish("b".into());
        hub.publish("c".into());
        let (waiting, mut rx) = hub.subscribe();
        assert_eq!(waiting, vec![Event::from("b"), Event::from("c")]);
        hub.publish("d".into());
        assert_eq!(rx.try_recv().unwrap(), Event::from("d"));
        let (again, _) = hub.subscribe();
        assert!(again.is_empty(), "the backlog is delivered once");
    }
}
