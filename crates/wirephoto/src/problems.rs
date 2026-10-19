//! The last warnings and errors the server logged, kept in memory for `wirephoto status`: what went wrong
//! lately, without digging through the log.

use std::collections::VecDeque;
use std::fmt::Write;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::Context;

/// How many are kept.
const KEEP: usize = 20;

static RECENT: Mutex<VecDeque<Problem>> = Mutex::new(VecDeque::new());

/// One warning or error.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Problem {
    /// Unix seconds.
    pub time: i64,
    /// `WARN` or `ERROR`.
    pub level: String,
    /// The part of the server it came from (`wirephoto::indexer`, ...).
    pub target: String,
    pub message: String,
    /// How many times in a row the same one was logged.
    pub repeats: u64,
}

/// The last warnings and errors, oldest first.
pub fn recent() -> Vec<Problem> {
    RECENT
        .lock()
        .map(|r| r.iter().cloned().collect())
        .unwrap_or_default()
}

/// Keeps a warning or error (the same one again counts as a repeat).
pub fn note(level: &str, target: &str, message: String) {
    let Ok(mut r) = RECENT.lock() else { return };
    let now = crate::util::now_secs();
    if let Some(last) = r.back_mut()
        && last.message == message
        && last.target == target
    {
        last.repeats += 1;
        last.time = now;
        return;
    }
    if r.len() == KEEP {
        r.pop_front();
    }
    r.push_back(Problem {
        time: now,
        level: level.into(),
        target: target.into(),
        message,
        repeats: 1,
    });
}

/// The tracing layer that feeds [`recent`].
pub struct Layer;

impl<S: Subscriber> tracing_subscriber::Layer<S> for Layer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let level = *event.metadata().level();
        if level > Level::WARN {
            return;
        }
        let mut v = Fields::default();
        event.record(&mut v);
        let mut message = v.message;
        if !v.rest.is_empty() {
            if !message.is_empty() {
                message.push(' ');
            }
            message.push_str(v.rest.trim_end());
        }
        note(level.as_str(), event.metadata().target(), message);
    }
}

#[derive(Default)]
struct Fields {
    message: String,
    rest: String,
}

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            let _ = write!(self.rest, "{}={value:?} ", field.name());
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            let _ = write!(self.rest, "{}={value} ", field.name());
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn repeats_fold() {
        super::note("WARN", "t", "first".into());
        super::note("WARN", "t", "again".into());
        super::note("WARN", "t", "again".into());
        let r = super::recent();
        let last = r.last().unwrap();
        assert_eq!((last.message.as_str(), last.repeats), ("again", 2));
    }
}
