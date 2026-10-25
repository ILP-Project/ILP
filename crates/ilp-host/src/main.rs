mod cli;
mod commands;
mod config;
mod engine;
mod exit;
mod http;
mod logging;
mod plugin_manager;
mod storage;

#[cfg(test)]
mod e2e_tests;

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::from(exit::SUCCESS as u8),
        Err(e) => {
            eprintln!("错误: {e:#}");
            std::process::ExitCode::from(exit::code_for(&e) as u8)
        }
    }
}

fn run() -> anyhow::Result<()> {
    let command = cli::parse();
    let config = config::Config::load("config.json")?;
    let _log_guard = logging::init(&config)?;

    // 配置里的异常值在日志起来之后重新报一遍，否则早先的告警会丢。
    config.report_problems();

    http::init(config.proxy.as_deref(), &config.no_proxy)?;
    tracing::debug!(
        max_workers = config.effective_max_workers(),
        sleep_time = config.sleep_time,
        "配置已加载"
    );

    match command {
        cli::Command::Plugin { sub } => plugin_manager::run(&config, sub),
        cli::Command::Get {
            kind,
            id,
            site_id,
            force_update,
        } => {
            let mut plugins = engine::load_plugins(&config)?;
            commands::do_get(&mut plugins, &config, &kind, &id, &site_id, force_update)
        }
        cli::Command::Search { keyword, site_id } => {
            let mut plugins = engine::load_plugins(&config)?;
            commands::do_search(&mut plugins, &config, &keyword, &site_id)
        }
        cli::Command::Version => commands::do_version(&config),
    }
}
