// SPDX-License-Identifier: GPL-3.0-or-later

use log::{LevelFilter, Log, Metadata, Record};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};

const MAX_LINES: usize = 2000;

static LOG_BUFFER: OnceLock<Arc<Mutex<VecDeque<String>>>> = OnceLock::new();

/// Returns all collected log lines joined by newlines.
pub fn get_logs() -> String {
    LOG_BUFFER
        .get()
        .and_then(|buf| buf.lock().ok())
        .map(|lines| lines.iter().cloned().collect::<Vec<_>>().join("\n"))
        .unwrap_or_default()
}

struct CollectingLogger {
    inner: env_logger::Logger,
    buffer: Arc<Mutex<VecDeque<String>>>,
}

impl Log for CollectingLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        // Accept everything up to Debug for the in-app buffer; terminal
        // output is filtered separately inside log().
        metadata.level() <= LevelFilter::Debug
    }

    fn log(&self, record: &Record) {
        // Forward to env_logger (respects RUST_LOG) for terminal output.
        if self.inner.enabled(record.metadata()) {
            self.inner.log(record);
        }

        // Always collect into ring buffer with a timestamp.
        let line = format!(
            "{} [{:<5}] {} — {}",
            chrono::Local::now().format("%H:%M:%S%.3f"),
            record.level(),
            record.target(),
            record.args()
        );
        if let Ok(mut buf) = self.buffer.lock() {
            buf.push_back(line);
            while buf.len() > MAX_LINES {
                buf.pop_front();
            }
        }
    }

    fn flush(&self) {
        self.inner.flush();
    }
}

/// Install the collecting logger. Must be called once before any logging.
pub fn init() {
    let buffer = Arc::new(Mutex::new(VecDeque::new()));
    LOG_BUFFER
        .set(buffer.clone())
        .expect("Logger already initialized");

    let inner = env_logger::Builder::from_default_env().build();
    let logger = CollectingLogger { inner, buffer };

    // Always route Debug+ to our buffer; terminal filter is env_logger's own.
    log::set_max_level(LevelFilter::Debug);
    log::set_boxed_logger(Box::new(logger)).expect("Failed to install logger");
}
