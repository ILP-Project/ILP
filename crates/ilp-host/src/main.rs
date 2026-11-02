mod cancel;
mod cli;
mod commands;
mod config;
mod diagnostics;
pub mod download_backend;
mod engine;
mod exit;
mod exporter;
mod help;
mod http;
mod logging;
mod messages;
mod plugin_manager;
pub mod plugin_registration;
mod replay;
mod signing;
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
    let parsed = cli::parse();
    if parsed.jsonl {
        std::env::set_var("ILP_JSONL", "1");
    }
    if parsed.verbose {
        std::env::set_var("RUST_LOG", "debug");
    }
    if parsed.quiet {
        std::env::set_var("ILP_QUIET", "1");
    }
    let command = parsed.command;
    let config = config::Config::load("config.json")?;
    let _log_guard = logging::init(&config, parsed.log_file.as_deref())?;
    commands::install_signal_handlers()?;
    commands::recover_interrupted_tasks(&config)?;

    // 配置里的异常值在日志起来之后重新报一遍，否则早先的告警会丢。
    config.report_problems();

    http::init(config.proxy.as_deref(), &config.no_proxy)?;
    tracing::debug!(
        max_workers = config.effective_max_workers(),
        sleep_time = config.sleep_time,
        "配置已加载"
    );

    match command {
        cli::Command::Resume { task_id } => {
            let mut plugins = engine::load_plugins(&config)?;
            commands::resume_download(&mut plugins, &config, task_id)
        }
        cli::Command::ResumeAll => commands::resume_all_downloads(&config),
        cli::Command::MigrationPreview => commands::migration_preview(&config),
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
        cli::Command::Update { id, site_id, apply } => {
            let mut plugins = engine::load_plugins(&config)?;
            commands::do_update(&mut plugins, &config, &id, &site_id, apply)
        }
        cli::Command::Doctor {
            bundle,
            with_manifest,
            yes,
        } => commands::do_doctor(&config, bundle, with_manifest, yes),
        cli::Command::Version => commands::do_version(&config),
        cli::Command::Completion { shell, output } => {
            help::write_completion(shell, output.as_deref())
        }
    }
}
