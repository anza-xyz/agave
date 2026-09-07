//! Wire protocol for the dashboard websocket.
//!
//! Every message is a JSON envelope with a `topic`, a `key` within it, and a
//! `value`. Retained messages carry state; the newest value per `(topic, key)`
//! is kept so a client connecting late is caught up in one shot. Ephemeral
//! messages describe an event and reach only the clients connected at the
//! time. A request carries an `id`, and its reply goes to that `id` alone.

use {
    serde::{Deserialize, Serialize},
    std::{
        collections::BTreeMap,
        sync::{Arc, Mutex},
    },
    tokio::sync::broadcast,
};

/// Ceiling on a single websocket message, both directions: soketto takes one
/// limit per connection, and a client's frame is buffered whole before any
/// smaller limit applies. The largest server message is the 512-slot overview
/// at under half a megabyte.
pub const MAX_MESSAGE: usize = 1024 * 1024;

/// Messages buffered per client before it counts as too slow and is dropped.
const BROADCAST_CAPACITY: usize = 8192;

/// The topics a client can receive. Here because they are part of the wire
/// format.
pub const TOPIC_SUMMARY: &str = "summary";
pub const TOPIC_EPOCH: &str = "epoch";
pub const TOPIC_SLOT: &str = "slot";
pub const TOPIC_PEERS: &str = "peers";

#[derive(Serialize)]
struct Envelope<'a, T> {
    topic: &'a str,
    key: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<u64>,
    value: T,
}

/// A request sent by a client. Unknown fields are ignored, so a client may send
/// arguments alongside these once a request exists that takes any.
#[derive(Deserialize)]
pub struct Request {
    pub topic: String,
    pub key: String,
    #[serde(default)]
    pub id: Option<u64>,
    /// Whatever the request carries, left unparsed: each request knows the shape
    /// of its own parameters.
    #[serde(default)]
    pub params: serde_json::Value,
}

/// A serialized, ready-to-send message. Serialization happens once, on the
/// publishing thread, and the resulting bytes are shared by every client.
pub type Message = Arc<str>;

pub fn encode<T: Serialize>(topic: &str, key: &str, value: &T) -> Message {
    encode_with_id(topic, key, None, value)
}

pub fn encode_with_id<T: Serialize>(topic: &str, key: &str, id: Option<u64>, value: &T) -> Message {
    let envelope = Envelope {
        topic,
        key,
        id,
        value,
    };
    // The only failure is a `Serialize` impl that errors. Falling back to null
    // keeps a bug in one topic from taking the feed down.
    match serde_json::to_string(&envelope) {
        Ok(json) => Arc::from(json.as_str()),
        Err(err) => {
            log::error!("dashboard: failed to encode {topic}.{key}: {err}");
            Arc::from(format!(r#"{{"topic":"{topic}","key":"{key}","value":null}}"#).as_str())
        }
    }
}

/// Fans messages out to connected clients and remembers the latest value of
/// every retained key so new connections can be caught up in one shot.
pub struct Publisher {
    retained: Mutex<BTreeMap<(&'static str, String), Message>>,
    sender: broadcast::Sender<Message>,
}

impl Default for Publisher {
    fn default() -> Self {
        Self::new()
    }
}

impl Publisher {
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            retained: Mutex::new(BTreeMap::new()),
            sender,
        }
    }

    /// Publish a value that should be replayed to clients connecting later.
    pub fn publish<T: Serialize>(&self, topic: &'static str, key: &str, value: &T) {
        let message = encode(topic, key, value);
        self.retained
            .lock()
            .unwrap()
            .insert((topic, key.to_string()), message.clone());
        // An error here only means nobody is listening yet.
        let _ = self.sender.send(message);
    }

    /// Publish a point-in-time event. Not replayed to future connections.
    pub fn publish_ephemeral<T: Serialize>(&self, topic: &'static str, key: &str, value: &T) {
        let _ = self.sender.send(encode(topic, key, value));
    }

    /// Updates what a future connection receives without sending anything now,
    /// for bulk snapshots whose incremental changes go out separately.
    pub fn retain_only<T: Serialize>(&self, topic: &'static str, key: &str, value: &T) {
        let message = encode(topic, key, value);
        self.retained
            .lock()
            .unwrap()
            .insert((topic, key.to_string()), message);
    }

    /// Everything a freshly connected client needs to render a full view.
    pub fn snapshot(&self) -> Vec<Message> {
        self.retained.lock().unwrap().values().cloned().collect()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Message> {
        self.sender.subscribe()
    }

    /// Websocket clients currently attached, so collection that only exists to be
    /// looked at can be skipped when nobody is.
    pub fn subscriber_count(&self) -> usize {
        self.sender.receiver_count()
    }
}

/// The last published value of a key, so collectors publish only on change.
pub struct Debounced<T> {
    last: Option<T>,
}

impl<T> Default for Debounced<T> {
    fn default() -> Self {
        Self { last: None }
    }
}

impl<T> Debounced<T> {
    /// The value most recently published, if any.
    pub fn last(&self) -> Option<&T> {
        self.last.as_ref()
    }
}

impl<T: Serialize + PartialEq> Debounced<T> {
    pub fn publish(&mut self, publisher: &Publisher, topic: &'static str, key: &str, value: T) {
        if self.last.as_ref() == Some(&value) {
            return;
        }
        publisher.publish(topic, key, &value);
        self.last = Some(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A value whose `Serialize` impl fails, standing in for a bug in one
    /// topic's payload.
    struct Unserializable;

    impl Serialize for Unserializable {
        fn serialize<S: serde::Serializer>(&self, _serializer: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom("this value cannot be encoded"))
        }
    }

    #[test]
    fn test_a_payload_that_cannot_encode_does_not_take_the_feed_down() {
        // One broken topic costs that topic and nothing else.
        let message = encode("summary", "broken", &Unserializable);
        assert_eq!(
            &*message,
            r#"{"topic":"summary","key":"broken","value":null}"#
        );
    }

    #[test]
    fn test_a_publisher_defaults_to_an_empty_one() {
        let publisher = Publisher::default();
        assert!(publisher.snapshot().is_empty());
        assert_eq!(publisher.subscriber_count(), 0);
    }

    #[test]
    fn test_a_debounce_remembers_what_it_last_sent() {
        // The collector reads this back to notice a vote that has moved, so it
        // has to hold the published value rather than merely a hash of it.
        let publisher = Publisher::new();
        let mut debounced: Debounced<u64> = Debounced::default();
        assert_eq!(debounced.last(), None);

        debounced.publish(&publisher, "summary", "root_slot", 7);
        assert_eq!(debounced.last(), Some(&7));

        debounced.publish(&publisher, "summary", "root_slot", 9);
        assert_eq!(debounced.last(), Some(&9));
    }

    #[test]
    fn test_retained_snapshot_replays_latest_value_only() {
        let publisher = Publisher::new();
        publisher.publish("summary", "root_slot", &1u64);
        publisher.publish("summary", "root_slot", &2u64);
        let snapshot = publisher.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert!(snapshot[0].contains(r#""value":2"#));
    }

    #[test]
    fn test_ephemeral_messages_are_not_replayed() {
        let publisher = Publisher::new();
        publisher.publish_ephemeral("slot", "update", &1u64);
        assert!(publisher.snapshot().is_empty());
    }

    #[test]
    fn test_retain_only_updates_snapshot_without_broadcasting() {
        let publisher = Publisher::new();
        let mut receiver = publisher.subscribe();
        publisher.retain_only("peers", "all", &[1u64, 2]);
        assert_eq!(publisher.snapshot().len(), 1);
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn test_subscribers_are_counted_while_they_are_attached() {
        let publisher = Publisher::new();
        assert_eq!(publisher.subscriber_count(), 0);
        let first = publisher.subscribe();
        let second = publisher.subscribe();
        assert_eq!(publisher.subscriber_count(), 2);
        drop(first);
        assert_eq!(publisher.subscriber_count(), 1);
        drop(second);
        assert_eq!(publisher.subscriber_count(), 0);
    }

    #[test]
    fn test_debounce_suppresses_unchanged_values() {
        let publisher = Publisher::new();
        let mut receiver = publisher.subscribe();
        let mut debounced = Debounced::default();
        debounced.publish(&publisher, "summary", "root_slot", 7u64);
        debounced.publish(&publisher, "summary", "root_slot", 7u64);
        assert!(receiver.try_recv().is_ok());
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn test_envelope_has_topic_key_and_value() {
        let message = encode("summary", "cluster", &"testnet");
        assert_eq!(
            &*message,
            r#"{"topic":"summary","key":"cluster","value":"testnet"}"#
        );
    }

    #[test]
    fn test_query_responses_carry_the_request_id() {
        let message = encode_with_id("summary", "ping", Some(42), &());
        assert_eq!(
            &*message,
            r#"{"topic":"summary","key":"ping","id":42,"value":null}"#
        );
    }
}
