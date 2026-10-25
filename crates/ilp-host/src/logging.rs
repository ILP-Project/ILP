use anyhow::{Context, Result};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::config::Config;

/// 持有它才能保证日志线程在退出前把缓冲刷完，别提前 drop。
pub struct LogGuard {
    _file: WorkerGuard,
}

pub fn init(config: &Config) -> Result<LogGuard> {
    let level = parse_level(&config.log_level);
    let log_dir = config.logs_dir();
    std::fs::create_dir_all(&log_dir)
        .with_context(|| format!("创建日志目录失败: {}", log_dir.display()))?;

    let file_appender = tracing_appender::rolling::daily(&log_dir, "ilp.log");
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(file_writer)
        .with_ansi(false)
        .with_target(true);

    let stderr_layer = config.log_to_stderr.then(|| {
        tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_ansi(true)
    });

    tracing_subscriber::registry()
        .with(level)
        .with(file_layer)
        .with(stderr_layer)
        .init();

    Ok(LogGuard { _file: guard })
}

fn parse_level(s: &str) -> LevelFilter {
    match s.to_ascii_lowercase().as_str() {
        "trace" => LevelFilter::TRACE,
        "debug" => LevelFilter::DEBUG,
        "warn" => LevelFilter::WARN,
        "error" => LevelFilter::ERROR,
        _ => LevelFilter::INFO,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_parsing_is_case_insensitive() {
        assert_eq!(parse_level("DEBUG"), LevelFilter::DEBUG);
        assert_eq!(parse_level("Warn"), LevelFilter::WARN);
    }

    #[test]
    fn unknown_level_falls_back_to_info() {
        assert_eq!(parse_level("verbose"), LevelFilter::INFO);
        assert_eq!(parse_level(""), LevelFilter::INFO);
    }
}
