//! Structured logging: JSON lines in production, a readable line otherwise.
//!
//! ```ignore
//! logger::info("Mailgun event", json!({ "event": event, "recipient": recipient }));
//! logger::error_with("Route handler error", json!({ "path": path }), &error);
//! ```

use std::fmt::Display;

use serde_json::{json, Map, Value};

use crate::env;
use crate::js;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    fn name(self) -> &'static str {
        match self {
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }

    fn color(self) -> &'static str {
        match self {
            Level::Debug => "\x1b[36m",
            Level::Info => "\x1b[32m",
            Level::Warn => "\x1b[33m",
            Level::Error => "\x1b[31m",
        }
    }
}

const RESET: &str = "\x1b[0m";

fn threshold() -> Level {
    match env::var("LOG_LEVEL").map(|v| v.to_lowercase()).as_deref() {
        Some("debug") => Level::Debug,
        Some("info") => Level::Info,
        Some("warn") => Level::Warn,
        Some("error") => Level::Error,
        _ if env::node_env().as_deref() == Some("test") => Level::Error,
        _ => Level::Info,
    }
}

fn non_empty_context(context: Value) -> Option<Map<String, Value>> {
    match context {
        Value::Object(map) if !map.is_empty() => Some(map),
        _ => None,
    }
}

fn emit(level: Level, message: &str, context: Value, error: Option<&dyn Display>) {
    if level < threshold() {
        return;
    }
    let timestamp = chrono::Utc::now();
    let context = non_empty_context(context);
    let error = error.map(|e| e.to_string());

    let line = if env::is_production() {
        let mut entry = Map::new();
        entry.insert("timestamp".into(), json!(js::iso_string(&timestamp)));
        entry.insert("level".into(), json!(level.name()));
        entry.insert("message".into(), json!(message));
        if let Some(context) = context {
            entry.insert("context".into(), Value::Object(context));
        }
        if let Some(error) = error {
            entry.insert("error".into(), json!({ "message": error }));
        }
        Value::Object(entry).to_string()
    } else {
        let mut line = format!(
            "{} {}{:<5}{} {}",
            timestamp.format("%-I:%M:%S %p"),
            level.color(),
            level.name().to_uppercase(),
            RESET,
            message
        );
        if let Some(context) = context {
            line.push(' ');
            line.push_str(&Value::Object(context).to_string());
        }
        if let Some(error) = error {
            line.push_str(&format!("\n  {}Error: {}{}", Level::Error.color(), error, RESET));
        }
        line
    };

    if level >= Level::Warn {
        eprintln!("{line}");
    } else {
        println!("{line}");
    }
}

pub fn debug(message: impl AsRef<str>, context: Value) {
    emit(Level::Debug, message.as_ref(), context, None);
}

pub fn info(message: impl AsRef<str>, context: Value) {
    emit(Level::Info, message.as_ref(), context, None);
}

pub fn warn(message: impl AsRef<str>, context: Value) {
    emit(Level::Warn, message.as_ref(), context, None);
}

pub fn warn_with(message: impl AsRef<str>, context: Value, error: &dyn Display) {
    emit(Level::Warn, message.as_ref(), context, Some(error));
}

pub fn error(message: impl AsRef<str>, context: Value) {
    emit(Level::Error, message.as_ref(), context, None);
}

pub fn error_with(message: impl AsRef<str>, context: Value, error: &dyn Display) {
    emit(Level::Error, message.as_ref(), context, Some(error));
}
