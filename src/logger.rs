//! Minimal stderr logger (`KTLS_LOG=error|warn|info|debug|trace`).

use std::io::Write;

struct StderrLogger {
    level: log::LevelFilter,
}

impl log::Log for StderrLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            let mut err = std::io::stderr().lock();
            let _ = writeln!(err, "[{} {}] {}", record.level(), record.target(), record.args());
        }
    }

    fn flush(&self) {}
}

pub fn init() {
    let level = match std::env::var("KTLS_LOG").unwrap_or_default().to_lowercase().as_str() {
        "error" => log::LevelFilter::Error,
        "warn" => log::LevelFilter::Warn,
        "debug" => log::LevelFilter::Debug,
        "trace" => log::LevelFilter::Trace,
        "off" => log::LevelFilter::Off,
        _ => log::LevelFilter::Info,
    };
    let logger: &'static StderrLogger = Box::leak(Box::new(StderrLogger { level }));
    if log::set_logger(logger).is_ok() {
        log::set_max_level(level);
    }
}
