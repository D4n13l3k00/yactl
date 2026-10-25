//! Bounded, process-local event history shared by hardware producers and API readers.
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Notify;

const CAPACITY: usize = 64;
const MAX_RECORD_BYTES: usize = 1024;

pub struct Feed {
    pub(crate) journal: Arc<Journal>,
    stream_id: String,
}
#[derive(Debug, Serialize)]
pub struct Page {
    pub stream_id: String,
    #[serde(flatten)]
    pub batch: Batch,
}
#[derive(Debug, PartialEq, Eq)]
pub enum ReadError {
    InvalidQuery,
    FutureCursor,
    StreamReset,
}
impl Feed {
    pub fn open() -> io::Result<Self> {
        let mut random = [0; 16];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
        Ok(Self {
            journal: Arc::new(Journal::default()),
            stream_id: random.iter().map(|b| format!("{b:02x}")).collect(),
        })
    }
    pub fn stream_id(&self) -> &str {
        &self.stream_id
    }
    pub async fn read(&self, query: Option<&str>) -> Result<Page, ReadError> {
        let mut after = None;
        let mut stream_id = None;
        let mut limit = None;
        let mut wait = None;
        if let Some(query) = query.filter(|query| !query.is_empty()) {
            if query.len() > 256 {
                return Err(ReadError::InvalidQuery);
            }
            // This contract uses only decimal integers and lowercase hex, so
            // percent encoding, signs, duplicates and unknown keys are rejected.
            for field in query.split('&') {
                let (key, value) = field.split_once('=').ok_or(ReadError::InvalidQuery)?;
                if key == "stream_id" {
                    if stream_id.is_some()
                        || value.len() != 32
                        || !value
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                    {
                        return Err(ReadError::InvalidQuery);
                    }
                    stream_id = Some(value);
                    continue;
                }
                let target = match key {
                    "after" => &mut after,
                    "limit" => &mut limit,
                    "wait_ms" => &mut wait,
                    _ => return Err(ReadError::InvalidQuery),
                };
                if target.is_some()
                    || value.is_empty()
                    || !value.bytes().all(|b| b.is_ascii_digit())
                {
                    return Err(ReadError::InvalidQuery);
                }
                *target = Some(value.parse::<u64>().map_err(|_| ReadError::InvalidQuery)?);
            }
        }
        if after.is_some() != stream_id.is_some() {
            return Err(ReadError::InvalidQuery);
        }
        let limit = limit.unwrap_or(16);
        let wait = wait.unwrap_or(0);
        if !(1..=16).contains(&limit) || wait > 2000 {
            return Err(ReadError::InvalidQuery);
        }
        if stream_id.is_some_and(|id| id != self.stream_id) {
            return Err(ReadError::StreamReset);
        }
        let batch = self
            .journal
            .poll(
                after.unwrap_or(0),
                limit as usize,
                Duration::from_millis(wait),
            )
            .await
            .map_err(|error| match error {
                Error::FutureCursor => ReadError::FutureCursor,
                _ => ReadError::InvalidQuery,
            })?;
        Ok(Page {
            stream_id: self.stream_id.clone(),
            batch,
        })
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Event {
    pub sequence: u64,
    pub timestamp_ms: u64,
    pub source: String,
    pub kind: String,
    pub data: Value,
}
#[derive(Debug, Serialize)]
pub struct Batch {
    pub events: Vec<Event>,
    pub cursor: u64,
    pub latest: u64,
    pub lost: u64,
    pub closed: bool,
}
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    InvalidEvent,
    InvalidLimit,
    InvalidWait,
    FutureCursor,
    Closed,
    SequenceExhausted,
}
#[derive(Default)]
struct State {
    events: VecDeque<Event>,
    latest: u64,
    closed: bool,
}
pub struct Journal {
    state: Mutex<State>,
    changed: Notify,
}
impl Default for Journal {
    fn default() -> Self {
        Self {
            state: Mutex::new(State::default()),
            changed: Notify::new(),
        }
    }
}

// Serialize into a counter, never an unbounded intermediate byte buffer. The
// bound includes JSON escaping and the complete record envelope.
struct Budget(usize);
impl Write for Budget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_sub(bytes.len())
            .ok_or_else(|| io::Error::other("event exceeds record limit"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.:".contains(&b))
}
impl Journal {
    pub fn publish(&self, source: &str, kind: &str, data: Value) -> Result<u64, Error> {
        if !valid_name(source) || !valid_name(kind) {
            return Err(Error::InvalidEvent);
        }
        let mut event = Event {
            sequence: u64::MAX,
            timestamp_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .min(u128::from(u64::MAX)) as u64,
            source: source.into(),
            kind: kind.into(),
            data,
        };
        serde_json::to_writer(&mut Budget(MAX_RECORD_BYTES), &event)
            .map_err(|_| Error::InvalidEvent)?;
        let sequence = {
            let mut state = self.state.lock().unwrap();
            if state.closed {
                return Err(Error::Closed);
            }
            let sequence = state
                .latest
                .checked_add(1)
                .ok_or(Error::SequenceExhausted)?;
            event.sequence = sequence;
            if state.events.len() == CAPACITY {
                state.events.pop_front();
            }
            state.events.push_back(event);
            state.latest = sequence;
            sequence
        };
        self.changed.notify_waiters();
        Ok(sequence)
    }
    fn snapshot(&self, after: u64, limit: usize) -> Result<Batch, Error> {
        let state = self.state.lock().unwrap();
        if after > state.latest {
            return Err(Error::FutureCursor);
        }
        let lost = state.events.front().map_or(0, |event| {
            event.sequence.saturating_sub(after).saturating_sub(1)
        });
        let events: Vec<_> = state
            .events
            .iter()
            .filter(|event| event.sequence > after)
            .take(limit)
            .cloned()
            .collect();
        let cursor = events.last().map_or(after, |event| event.sequence);
        Ok(Batch {
            events,
            cursor,
            latest: state.latest,
            lost,
            closed: state.closed,
        })
    }
    /// The caller must bound admitted readers (the HTTP layer has connection
    /// admission). Readers own no persistent queue or cursor in this journal.
    pub async fn poll(&self, after: u64, limit: usize, wait: Duration) -> Result<Batch, Error> {
        if !(1..=16).contains(&limit) {
            return Err(Error::InvalidLimit);
        }
        if wait > Duration::from_secs(2) {
            return Err(Error::InvalidWait);
        }
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            // Register before checking state: publication between checking and
            // awaiting must not leave a reader asleep until the deadline.
            notified.as_mut().enable();
            let batch = self.snapshot(after, limit)?;
            if !batch.events.is_empty() || batch.closed || tokio::time::Instant::now() >= deadline {
                return Ok(batch);
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.snapshot(after, limit);
            }
        }
    }
    pub fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.changed.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    #[tokio::test]
    async fn feed_pages_keep_cursor_and_generation_while_rejecting_ambiguous_queries() {
        let feed = Feed::open().unwrap();
        feed.journal
            .publish("input:test", "key", json!({"value":1}))
            .unwrap();
        feed.journal
            .publish("input:test", "key", json!({"value":0}))
            .unwrap();
        let page = feed.read(Some("limit=1")).await.unwrap();
        assert_eq!((page.batch.cursor, page.batch.latest), (1, 2));
        let query = format!("after=1&stream_id={}", page.stream_id);
        let next = feed.read(Some(&query)).await.unwrap();
        assert_eq!(next.stream_id, page.stream_id);
        assert_eq!(next.batch.events[0].data, json!({"value":0}));
        assert_eq!(next.batch.cursor, 2);
        for query in [
            "after=0",
            "limit=1&limit=2",
            "limit=+1",
            "limit=%31",
            "unknown=1",
            "wait_ms=2001",
            "limit=0",
            "limit=17",
            "after=18446744073709551616",
            "limit=1&",
            "&limit=1",
        ] {
            assert_eq!(
                feed.read(Some(query)).await.unwrap_err(),
                ReadError::InvalidQuery,
                "{query}"
            );
        }
        assert_eq!(
            feed.read(Some(&format!("after=3&stream_id={}", page.stream_id)))
                .await
                .unwrap_err(),
            ReadError::FutureCursor
        );
        let other = Feed::open().unwrap();
        assert_ne!(other.stream_id(), feed.stream_id());
        assert_eq!(
            other.read(Some(&query)).await.unwrap_err(),
            ReadError::StreamReset
        );
    }

    #[tokio::test]
    async fn slow_readers_receive_loss_count_and_can_page_without_skipping() {
        let journal = Journal::default();
        for value in 1..=70 {
            assert_eq!(
                journal.publish("input:test", "key", json!({"value":value})),
                Ok(value)
            );
        }
        let first = journal.poll(0, 2, Duration::ZERO).await.unwrap();
        assert_eq!(first.lost, 6);
        assert_eq!(first.latest, 70);
        assert_eq!(first.cursor, 8);
        assert_eq!(
            first.events.iter().map(|e| e.sequence).collect::<Vec<_>>(),
            [7, 8]
        );
        assert_eq!(first.events[0].data, json!({"value":7}));
        assert_eq!(first.events[0].source, "input:test");
        let next = journal
            .poll(first.cursor, 16, Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(next.lost, 0);
        assert_eq!(next.events.first().unwrap().sequence, 9);
        assert_eq!(next.cursor, 24);
        assert_eq!(
            journal.poll(71, 1, Duration::ZERO).await.unwrap_err(),
            Error::FutureCursor
        );
    }

    #[tokio::test]
    async fn rejected_events_do_not_consume_sequence_or_evict_history() {
        let journal = Journal::default();
        assert_eq!(
            journal.publish("input:test", "key", json!({"value":1})),
            Ok(1)
        );
        for (source, kind, data) in [
            ("", "key", json!(0)),
            ("input:test", "bad kind", json!(0)),
            ("input:test", "key", json!("x".repeat(1025))),
            ("input:test", "key", json!("\n".repeat(600))),
        ] {
            assert_eq!(
                journal.publish(source, kind, data),
                Err(Error::InvalidEvent)
            );
        }
        assert_eq!(
            journal.publish("input:test", "key", json!({"value":2})),
            Ok(2)
        );
        let batch = journal.poll(0, 16, Duration::ZERO).await.unwrap();
        assert_eq!(batch.events.len(), 2);
        assert_eq!(batch.latest, 2);
        assert_eq!(batch.lost, 0);
        assert_eq!(
            journal.poll(0, 0, Duration::ZERO).await.unwrap_err(),
            Error::InvalidLimit
        );
        assert_eq!(
            journal.poll(0, 17, Duration::ZERO).await.unwrap_err(),
            Error::InvalidLimit
        );
        assert_eq!(
            journal
                .poll(0, 1, Duration::from_millis(2001))
                .await
                .unwrap_err(),
            Error::InvalidWait
        );
    }

    #[tokio::test]
    async fn publication_wakes_all_independent_readers() {
        let journal = Arc::new(Journal::default());
        let mut readers = Vec::new();
        for _ in 0..8 {
            let journal = journal.clone();
            readers.push(tokio::spawn(async move {
                journal.poll(0, 16, Duration::from_secs(2)).await
            }));
        }
        tokio::task::yield_now().await;
        journal
            .publish("input:test", "switch", json!({"code":14,"value":1}))
            .unwrap();
        for reader in readers {
            let batch = tokio::time::timeout(Duration::from_millis(200), reader)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(batch.events.len(), 1);
            assert_eq!(batch.events[0].data, json!({"code":14,"value":1}));
        }
    }

    #[tokio::test]
    async fn canceled_reader_does_not_consume_events_and_close_wakes_waiters() {
        let journal = Arc::new(Journal::default());
        let other = journal.clone();
        let reader = tokio::spawn(async move { other.poll(0, 1, Duration::from_secs(2)).await });
        tokio::task::yield_now().await;
        reader.abort();
        assert!(reader.await.unwrap_err().is_cancelled());
        journal
            .publish("firmware:test", "state", json!({"active":true}))
            .unwrap();
        assert_eq!(journal.poll(0, 1, Duration::ZERO).await.unwrap().cursor, 1);
        let other = journal.clone();
        let reader = tokio::spawn(async move { other.poll(1, 1, Duration::from_secs(2)).await });
        tokio::task::yield_now().await;
        journal.close();
        let batch = tokio::time::timeout(Duration::from_millis(200), reader)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(batch.closed);
        assert!(batch.events.is_empty());
        assert_eq!(batch.cursor, 1);
        assert_eq!(
            journal.publish("input:test", "key", json!(1)),
            Err(Error::Closed)
        );
        assert_eq!(
            journal
                .poll(0, 16, Duration::ZERO)
                .await
                .unwrap()
                .events
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn idle_poll_returns_at_its_deadline_without_creating_events() {
        let journal = Journal::default();
        let start = tokio::time::Instant::now();
        let batch = tokio::time::timeout(
            Duration::from_millis(300),
            journal.poll(0, 1, Duration::from_millis(30)),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(start.elapsed() >= Duration::from_millis(30));
        assert!(batch.events.is_empty());
        assert_eq!((batch.cursor, batch.latest, batch.lost), (0, 0, 0));
        assert!(!batch.closed);
    }
}
