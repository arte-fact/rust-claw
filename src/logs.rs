use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use tokio::sync::broadcast;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

/// How many recent records the admin viewer keeps in memory. The full history
/// still lands in `logs/claw.log`; this buffer is just the live tail.
pub const DEFAULT_CAPACITY: usize = 1000;
const STREAM_CAPACITY: usize = 256;

#[derive(Debug, Clone, Serialize)]
pub struct LogRecord {
    pub seq: u64,
    /// RFC3339 UTC, e.g. `2026-06-12T11:43:29.325Z`.
    pub ts: String,
    pub level: &'static str,
    pub target: String,
    pub message: String,
}

impl LogRecord {
    /// The `HH:MM:SS` slice of the RFC3339 timestamp, for compact display.
    #[must_use]
    pub fn time(&self) -> &str {
        self.ts.get(11..19).unwrap_or(&self.ts)
    }
}

/// A bounded ring of recent log records plus a broadcast of new ones, filled by
/// [`LogLayer`] and read by the `/admin/logs` page + its SSE stream (M13).
pub struct LogBuffer {
    inner: Mutex<VecDeque<LogRecord>>,
    tx: broadcast::Sender<LogRecord>,
    capacity: usize,
    seq: AtomicU64,
}

impl LogBuffer {
    #[must_use]
    pub fn new(capacity: usize) -> Arc<Self> {
        let (tx, _) = broadcast::channel(STREAM_CAPACITY);
        Arc::new(Self {
            inner: Mutex::new(VecDeque::with_capacity(capacity.min(1024))),
            tx,
            capacity: capacity.max(1),
            seq: AtomicU64::new(0),
        })
    }

    fn push(&self, level: &'static str, target: &str, message: String) {
        let record = LogRecord {
            seq: self.seq.fetch_add(1, Ordering::Relaxed),
            ts: jiff::Timestamp::now().to_string(),
            level,
            target: target.to_owned(),
            message,
        };
        if let Ok(mut buffer) = self.inner.lock() {
            buffer.push_back(record.clone());
            while buffer.len() > self.capacity {
                buffer.pop_front();
            }
        }
        // Send fails only when no SSE client is connected — that's fine.
        let _ = self.tx.send(record);
    }

    #[must_use]
    pub fn snapshot(&self) -> Vec<LogRecord> {
        self.inner
            .lock()
            .map(|buffer| buffer.iter().cloned().collect())
            .unwrap_or_default()
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<LogRecord> {
        self.tx.subscribe()
    }
}

/// A `tracing` layer that copies each event (after the global `EnvFilter`) into a
/// [`LogBuffer`]. Added to the subscriber in `logging::init`, before any layer
/// that consumes it.
pub struct LogLayer {
    buffer: Arc<LogBuffer>,
}

impl LogLayer {
    #[must_use]
    pub fn new(buffer: Arc<LogBuffer>) -> Self {
        Self { buffer }
    }
}

impl<S: Subscriber> Layer<S> for LogLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let metadata = event.metadata();
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        self.buffer.push(
            level_str(metadata.level()),
            metadata.target(),
            visitor.finish(),
        );
    }
}

fn level_str(level: &Level) -> &'static str {
    match *level {
        Level::ERROR => "ERROR",
        Level::WARN => "WARN",
        Level::INFO => "INFO",
        Level::DEBUG => "DEBUG",
        Level::TRACE => "TRACE",
    }
}

/// Flattens an event to text: the `message` field, then any other fields as
/// `key=value` (so `tracing::info!(session = %id, "started")` keeps the id).
#[derive(Default)]
struct MessageVisitor {
    message: String,
    fields: String,
}

impl MessageVisitor {
    fn finish(mut self) -> String {
        self.message.push_str(&self.fields);
        self.message
    }
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write;
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            let _ = write!(self.fields, " {}={value:?}", field.name());
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        use std::fmt::Write;
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            let _ = write!(self.fields, " {}={value}", field.name());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_keeps_only_the_most_recent_within_capacity() {
        let buffer = LogBuffer::new(3);
        for n in 0..5 {
            buffer.push("INFO", "test", format!("msg {n}"));
        }
        let snapshot = buffer.snapshot();
        let messages: Vec<_> = snapshot.iter().map(|r| r.message.as_str()).collect();
        assert_eq!(messages, ["msg 2", "msg 3", "msg 4"]);
    }

    #[test]
    fn seq_increases_monotonically_across_eviction() {
        let buffer = LogBuffer::new(2);
        for _ in 0..4 {
            buffer.push("WARN", "test", "x".to_owned());
        }
        let seqs: Vec<_> = buffer.snapshot().iter().map(|r| r.seq).collect();
        assert_eq!(seqs, [2, 3]);
    }

    #[tokio::test]
    async fn subscribers_receive_pushed_records() {
        let buffer = LogBuffer::new(8);
        let mut rx = buffer.subscribe();
        buffer.push("ERROR", "claw::web", "boom".to_owned());
        let record = rx.recv().await.expect("record");
        assert_eq!(record.level, "ERROR");
        assert_eq!(record.target, "claw::web");
        assert_eq!(record.message, "boom");
    }
}
