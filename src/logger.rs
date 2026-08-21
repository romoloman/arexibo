// Xibo player Rust implementation, (c) 2022-2024 Georg Brandl.
// Licensed under the GNU AGPL, version 3 or later.

//! Xibo logger.

use time::OffsetDateTime;
use parking_lot::Mutex;

/// A single cached log entry.
pub struct LogEntry {
    pub date: OffsetDateTime,
    pub category: &'static str,
    pub message: String,
}


static LOG_ENTRIES: Mutex<Vec<LogEntry>> = Mutex::new(Vec::new());

/// Xibo logger, logs to console and stores entries for transfer to
/// the display.
pub struct Logger;

impl log::Log for Logger {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        // filter out messages not from our modules
        let path = record.module_path().unwrap_or("");
        if !path.starts_with("arexibo") {
            return;
        }

        // BUG fix (found from a real crash report): formatting
        // record.args() TWICE (once for console, once for the stashed
        // entry below) used to panic outright if a log:: call
        // interpolated a non-idempotent Display value -- itertools's
        // own `Format` combinator (`.iter().format(", ")`) explicitly
        // supports being formatted only once, and panics with "Format:
        // was already formatted once" on a second attempt. Most
        // Display impls (numbers, strings, etc.) don't have this
        // restriction, so this went unnoticed until a real call site
        // happened to interpolate one directly. Formatting once here,
        // into a plain (always-idempotent) String, and reusing that
        // for both destinations, fixes this for every current AND
        // future log:: call site in this codebase -- not just the one
        // that happened to trigger the actual crash.
        let formatted = record.args().to_string();

        // print to console
        println!("{:5}: [{}] {}", record.level(), path, formatted);

        // add to stashed entries for submission to CMS
        let mut entries = LOG_ENTRIES.lock();
        // avoid taking up arbitrary amounts of memory
        if entries.len() > 1000 {
            entries.drain(0..500).for_each(drop);
        }
        entries.push(LogEntry {
            date: OffsetDateTime::now_local().unwrap(),
            category: record.level().as_str(),
            message: formatted,
        });
    }

    fn flush(&self) {}
}

pub fn pop_entries() -> Vec<LogEntry> {
    std::mem::take(&mut LOG_ENTRIES.lock())
}

#[cfg(test)]
mod tests {
    use super::*;
    use itertools::Itertools;
    use log::Log as _;

    #[test]
    fn logging_a_non_idempotent_display_value_does_not_panic() {
        // Regression test for a real crash report: this codebase's own
        // itertools::Format value (`.iter().format(", ")`) explicitly
        // supports being formatted only once -- a second attempt
        // panics with "Format: was already formatted once". Before the
        // fix, Logger::log() itself formatted record.args() TWICE
        // (once for console, once for the stashed entry), which
        // crashed outright the moment any log:: call site interpolated
        // such a value directly (as one real call site in mainloop.rs
        // did). Fixed at the logger level (format once, reuse the
        // resulting String for both destinations) rather than only at
        // that one call site, so this can never recur for any other
        // current or future log:: call in this codebase.
        let items = vec![925, 913, 805];
        let formatted_once = items.iter().format(", ");

        // Must not panic -- this is the actual assertion. Calling this
        // at all (with a genuinely single-use Format value inside
        // args()) is the regression test; if Logger::log() were to
        // format args() more than once internally, this line itself
        // would panic with the exact real crash message. Record
        // construction and the log() call are kept in one expression
        // (no intermediate `let record = ...`) since Arguments borrows
        // from temporaries created by format_args! itself.
        Logger.log(&log::Record::builder()
            .args(format_args!("layouts: {formatted_once}"))
            .level(log::Level::Warn)
            .module_path(Some("arexibo::logger"))
            .build());
    }
}
