use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::fs;
use std::path::Path;

use crate::cli::PluginCommand;

const PLUGINS_DIR: &str = "plugins";
const DISABLED_DIR: &str = "plugins/disabled";

#[derive(Deserialize)]
struct PluginJson {
    name: String,
    url: String,
}

pub fn run(cmd: PluginCommand) -> Result<()> {
    match cmd {
        PluginCommand::List => list(),
        PluginCommand::Install { source } => install(&source),
        PluginCommand::Uninstall { name } => uninstall(&name),
        PluginCommand::Enable { name } => enable(&name),
        PluginCommand::Disable { name } => disable(&name),
    }
}

fn list() -> Result<()> {
    let enabled = list_wasm(PLUGINS_DIR);
    let disabled = list_wasm(DISABLED_DIR);
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

fn list_wasm(dir: &str) -> Vec<String> {
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

fn install(source: &str) -> Result<()> {
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
    fs::create_dir_all(PLUGINS_DIR)?;
    let dest = format!("{PLUGINS_DIR}/{}.wasm", pj.name);
    fs::write(&dest, bytes)?;
    println!("已安装插件 {} 到 {dest}", pj.name);
    Ok(())
}

fn uninstall(name: &str) -> Result<()> {
    let enabled = format!("{PLUGINS_DIR}/{name}.wasm");
    let disabled = format!("{DISABLED_DIR}/{name}.wasm");
    if Path::new(&enabled).exists() {
        fs::remove_file(&enabled)?;
        println!("已卸载插件 {name}");
        Ok(())
    } else if Path::new(&disabled).exists() {
        fs::remove_file(&disabled)?;
        println!("已卸载插件 {name}");
        Ok(())
    } else {
        Err(anyhow!("插件不存在: {name}"))
    }
}

fn enable(name: &str) -> Result<()> {
    let disabled = format!("{DISABLED_DIR}/{name}.wasm");
    let enabled = format!("{PLUGINS_DIR}/{name}.wasm");
    if Path::new(&disabled).exists() {
        fs::create_dir_all(PLUGINS_DIR)?;
        fs::rename(&disabled, &enabled)?;
        println!("已启用插件 {name}");
        Ok(())
    } else {
        Err(anyhow!("未找到禁用插件: {name}"))
    }
}

fn disable(name: &str) -> Result<()> {
    let enabled = format!("{PLUGINS_DIR}/{name}.wasm");
    let disabled = format!("{DISABLED_DIR}/{name}.wasm");
    if Path::new(&enabled).exists() {
        fs::create_dir_all(DISABLED_DIR)?;
        fs::rename(&enabled, &disabled)?;
        println!("已禁用插件 {name}");
        Ok(())
    } else {
        Err(anyhow!("未找到启用插件: {name}"))
    }
}
