//! In-Memory Flight Recorder (`P0-T11` / `ADR-016` / `Vortex-Architecture-Design.md` §22.2).
//!
//! Retains the most recent `capacity` (default: 2,000) `DEBUG+` events in a fixed-memory
//! ring buffer (~< 2 MiB) and flushes them automatically whenever a critical `ERROR` or
//! panic occurs, providing rich pre-failure context without bloating normal `INFO` logs.

use std::collections::{BTreeMap, VecDeque};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

use crate::redaction::{is_sensitive_key, redact_sensitive_text};

/// Default capacity of the in-memory Flight Recorder ring buffer (`P0-T11`).
pub const DEFAULT_FLIGHT_RECORDER_CAPACITY: usize = 2_000;

/// Single structured event captured inside the [`FlightRecorder`] ring buffer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlightEvent {
    /// RFC-3339 UTC timestamp.
    pub timestamp: String,
    /// Log severity (`TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR`).
    pub level: String,
    /// Rust module target.
    pub target: String,
    /// Redacted message string.
    pub message: String,
    /// Structured span and event fields (redacted).
    pub fields: BTreeMap<String, String>,
}

/// Thread-safe bounded ring buffer capturing recent `DEBUG+` diagnostic events.
#[derive(Clone)]
pub struct FlightRecorder {
    inner: Arc<Mutex<VecDeque<FlightEvent>>>,
    capacity: usize,
    auto_dump_path: Option<PathBuf>,
}

impl fmt::Debug for FlightRecorder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FlightRecorder")
            .field("capacity", &self.capacity)
            .field("len", &self.len())
            .finish_non_exhaustive()
    }
}

use std::fmt;

impl FlightRecorder {
    /// Creates a new in-memory [`FlightRecorder`] with `capacity` ring-buffer slots.
    #[must_use]
    pub fn new(capacity: usize, auto_dump_path: Option<PathBuf>) -> Self {
        let cap = capacity.max(1);
        Self {
            inner: Arc::new(Mutex::new(VecDeque::with_capacity(cap.min(2_048)))),
            capacity: cap,
            auto_dump_path,
        }
    }

    /// Appends an event, evicting the oldest entry when full.
    pub fn record(&self, event: FlightEvent) {
        let is_error = event.level == "ERROR";
        if let Ok(mut guard) = self.inner.lock() {
            if guard.len() >= self.capacity {
                let _ = guard.pop_front();
            }
            guard.push_back(event);
        }
        if is_error && let Some(ref dump_path) = self.auto_dump_path {
            let _ = self.dump_jsonl_to(dump_path);
        }
    }

    /// Returns the number of events currently stored in the ring buffer.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.lock().map_or(0, |g| g.len())
    }

    /// Returns `true` if the ring buffer has no stored events.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns a chronological snapshot of all buffered events.
    #[must_use]
    pub fn snapshot(&self) -> Vec<FlightEvent> {
        self.inner
            .lock()
            .map_or_else(|_| Vec::new(), |g| g.iter().cloned().collect())
    }

    /// Serializes all buffered events as JSONL lines into `path`.
    ///
    /// # Errors
    /// Returns an I/O error if the dump file cannot be created or written.
    pub fn dump_jsonl_to(&self, path: &Path) -> std::io::Result<usize> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let events = self.snapshot();
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)?;
        for evt in &events {
            if let Ok(line) = serde_json::to_string(evt) {
                writeln!(file, "{line}")?;
            }
        }
        file.flush()?;
        Ok(events.len())
    }
}

/// `tracing_subscriber` layer that feeds `DEBUG` and higher events into a [`FlightRecorder`].
#[derive(Clone, Debug)]
pub struct FlightRecorderLayer {
    recorder: FlightRecorder,
}

impl FlightRecorderLayer {
    /// Wraps a [`FlightRecorder`] as a tracing layer.
    #[must_use]
    pub const fn new(recorder: FlightRecorder) -> Self {
        Self { recorder }
    }
}

#[derive(Default)]
struct FieldCollector {
    message: String,
    fields: BTreeMap<String, String>,
}

impl Visit for FieldCollector {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if is_sensitive_key(field.name()) {
            self.fields
                .insert(field.name().to_owned(), "[REDACTED]".to_owned());
            return;
        }
        let clean = redact_sensitive_text(&format!("{value:?}"));
        if field.name() == "message" {
            self.message = clean;
        } else {
            self.fields.insert(field.name().to_owned(), clean);
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if is_sensitive_key(field.name()) {
            self.fields
                .insert(field.name().to_owned(), "[REDACTED]".to_owned());
            return;
        }
        let clean = redact_sensitive_text(value);
        if field.name() == "message" {
            self.message = clean;
        } else {
            self.fields.insert(field.name().to_owned(), clean);
        }
    }
}

/// Extension stored per span to capture structured span fields such as `trace_id` and `download_id`.
#[derive(Default)]
struct SpanFields(BTreeMap<String, String>);

impl<S> Layer<S> for FlightRecorderLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        if let Some(span) = ctx.span(id) {
            let mut collector = FieldCollector::default();
            attrs.record(&mut collector);
            span.extensions_mut().insert(SpanFields(collector.fields));
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let meta = event.metadata();
        // Record DEBUG, INFO, WARN, ERROR (skip noisy TRACE in flight recorder).
        if *meta.level() > Level::DEBUG {
            return;
        }

        let mut collector = FieldCollector::default();
        event.record(&mut collector);

        // Merge enclosing span fields (e.g. trace_id, download_id, worker_id).
        if let Some(scope) = ctx.event_scope(event) {
            for span in scope.from_root() {
                if let Some(ext) = span.extensions().get::<SpanFields>() {
                    for (k, v) in &ext.0 {
                        collector
                            .fields
                            .entry(k.clone())
                            .or_insert_with(|| v.clone());
                    }
                }
            }
        }

        let timestamp = OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned());

        self.recorder.record(FlightEvent {
            timestamp,
            level: meta.level().as_str().to_owned(),
            target: meta.target().to_owned(),
            message: collector.message,
            fields: collector.fields,
        });
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn ring_buffer_evicts_oldest_and_auto_dumps_on_error() {
        let tmp = tempfile::tempdir().unwrap();
        let dump_path = tmp.path().join("flight-dump.jsonl");
        let recorder = FlightRecorder::new(3, Some(dump_path.clone()));

        for idx in 0..5 {
            recorder.record(FlightEvent {
                timestamp: "2026-11-25T20:00:00Z".to_owned(),
                level: if idx == 4 { "ERROR" } else { "DEBUG" }.to_owned(),
                target: "vortex::test".to_owned(),
                message: format!("event-{idx}"),
                fields: BTreeMap::new(),
            });
        }

        let snap = recorder.snapshot();
        assert_eq!(snap.len(), 3);
        assert_eq!(snap.first().unwrap().message, "event-2");
        assert_eq!(snap.last().unwrap().message, "event-4");

        // ERROR event must have triggered an automatic JSONL dump
        assert!(dump_path.is_file());
        let dumped = std::fs::read_to_string(&dump_path).unwrap();
        assert!(dumped.contains("event-2"));
        assert!(dumped.contains("event-4"));
    }
}
