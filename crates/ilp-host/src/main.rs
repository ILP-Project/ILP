mod cli;
mod commands;
mod config;
mod engine;
mod http;
mod plugin_manager;
mod storage;

use anyhow::Result;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let command = cli::parse();
    let config = config::Config::load("config.json")?;

    match command {
        cli::Command::Plugin { sub } => plugin_manager::run(sub),
        cli::Command::Get {
            kind,
            id,
            site_id,
            force_update,
        } => {
            let mut plugins = engine::load_plugins()?;
            commands::do_get(&mut plugins, &config, &kind, &id, &site_id, force_update)
        }
        cli::Command::Search { keyword, site_id } => {
            let mut plugins = engine::load_plugins()?;
            commands::do_search(&mut plugins, &keyword, &site_id)
        }
    }
}
