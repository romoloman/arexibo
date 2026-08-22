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

        // Format once, not twice (console + stashed entry) -- some
        // Display impls (e.g. itertools::Format) panic if formatted
        // more than once.
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
        // Regression test: itertools::Format panics if formatted
        // twice -- Logger::log() used to do exactly that.
        let items = [925, 913, 805];
        let formatted_once = items.iter().format(", ");

        // Must not panic. Kept as one expression since Arguments
        // borrows from format_args!'s own temporaries.
        Logger.log(&log::Record::builder()
            .args(format_args!("layouts: {formatted_once}"))
            .level(log::Level::Warn)
            .module_path(Some("arexibo::logger"))
            .build());
    }
}
