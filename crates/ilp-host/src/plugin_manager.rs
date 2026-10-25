use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::fs;
use std::path::Path;

use crate::cli::PluginCommand;
use crate::config::Config;

#[derive(Deserialize)]
struct PluginJson {
    name: String,
    url: String,
}

pub fn run(config: &Config, cmd: PluginCommand) -> Result<()> {
    match cmd {
        PluginCommand::List => list(config),
        PluginCommand::Install { source } => install(config, &source),
        PluginCommand::Uninstall { name } => uninstall(config, &name),
        PluginCommand::Enable { name } => enable(config, &name),
        PluginCommand::Disable { name } => disable(config, &name),
    }
}

fn list(config: &Config) -> Result<()> {
    let enabled = list_wasm(&config.plugins_dir());
    let disabled = list_wasm(&config.disabled_plugins_dir());
    println!("已启用插件:");
    for name in &enabled {
        println!("  {name}");
    }
    println!("已禁用插件:");
    for name in &disabled {
        println!("  {name}");
    }
    Ok(())
}

fn list_wasm(dir: &Path) -> Vec<String> {
    let mut names = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("wasm") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    names.push(stem.to_string());
                }
            }
        }
    }
    names
}

fn install(config: &Config, source: &str) -> Result<()> {
    let (owner, repo) = source
        .split_once('/')
        .ok_or_else(|| anyhow!("source 应为 owner/repo 格式"))?;
    let json_url =
        format!("https://raw.githubusercontent.com/{owner}/{repo}/refs/heads/main/plugin.json");
    let text = crate::http::get_text(&json_url)?;
    let pj: PluginJson = serde_json::from_str(&text).context("解析 plugin.json 失败")?;
    let bytes = crate::http::get_bytes(&pj.url)?;
    if bytes.is_empty() {
        return Err(anyhow!("下载产物为空: {}", pj.url));
    }
    let dir = config.plugins_dir();
    fs::create_dir_all(&dir)?;
    let dest = dir.join(format!("{}.wasm", pj.name));
    fs::write(&dest, bytes)?;
    println!("已安装插件 {} 到 {}", pj.name, dest.display());
    Ok(())
}

fn uninstall(config: &Config, name: &str) -> Result<()> {
    let enabled = config.plugins_dir().join(format!("{name}.wasm"));
    let disabled = config.disabled_plugins_dir().join(format!("{name}.wasm"));
    if enabled.exists() {
        fs::remove_file(&enabled)?;
        println!("已卸载插件 {name}");
        Ok(())
    } else if disabled.exists() {
        fs::remove_file(&disabled)?;
        println!("已卸载插件 {name}");
        Ok(())
    } else {
        Err(anyhow!("插件不存在: {name}"))
    }
}

fn enable(config: &Config, name: &str) -> Result<()> {
    let disabled = config.disabled_plugins_dir().join(format!("{name}.wasm"));
    let enabled = config.plugins_dir().join(format!("{name}.wasm"));
    if disabled.exists() {
        fs::create_dir_all(config.plugins_dir())?;
        fs::rename(&disabled, &enabled)?;
        println!("已启用插件 {name}");
        Ok(())
    } else {
        Err(anyhow!("未找到禁用插件: {name}"))
    }
}

fn disable(config: &Config, name: &str) -> Result<()> {
    let enabled = config.plugins_dir().join(format!("{name}.wasm"));
    let disabled = config.disabled_plugins_dir().join(format!("{name}.wasm"));
    if enabled.exists() {
        fs::create_dir_all(config.disabled_plugins_dir())?;
        fs::rename(&enabled, &disabled)?;
        println!("已禁用插件 {name}");
        Ok(())
    } else {
        Err(anyhow!("未找到启用插件: {name}"))
    }
}
