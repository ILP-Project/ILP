use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

use crate::cli::PluginCommand;
use crate::config::Config;

/// 插件安装信息。`url` 旧版按 OS 分平台给，这里接受两种形态：
/// 直接字符串，或 {os: url} 映射（取当前平台的，取不到再用 linux 兜底）。
#[derive(Deserialize)]
struct PluginJson {
    name: String,
    #[serde(alias = "download_url")]
    url: UrlField,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum UrlField {
    Single(String),
    ByOs(std::collections::HashMap<String, String>),
}

impl UrlField {
    fn pick(&self) -> Option<&str> {
        match self {
            UrlField::Single(u) => Some(u.as_str()),
            UrlField::ByOs(m) => {
                // 各仓库的键大小写不一，官方模板用的是 "Linux"/"Windows"。
                let os = std::env::consts::OS;
                let find = |want: &str| {
                    m.iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case(want))
                        .map(|(_, v)| v.as_str())
                };
                find(os)
                    .or_else(|| find("linux"))
                    .or_else(|| m.values().next().map(|v| v.as_str()))
            }
        }
    }
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

    if enabled.is_empty() && disabled.is_empty() {
        println!("没有已安装的插件。用 `ilp plugin install <owner/repo>` 安装。");
        return Ok(());
    }
    if !enabled.is_empty() {
        println!("已启用:");
        for name in &enabled {
            println!("  {name}");
        }
    }
    if !disabled.is_empty() {
        println!("已禁用:");
        for name in &disabled {
            println!("  {name}");
        }
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
    names.sort();
    names
}

/// 解析并校验 `owner/repo`。
///
/// 这两个值会被拼进 URL 和文件名，不校验就等于把路径穿越和任意 URL 交给用户。
fn parse_source(source: &str) -> Result<(String, String)> {
    let (owner, repo) = source
        .split_once('/')
        .ok_or_else(|| anyhow!("安装源应为 owner/repo 格式，例如 RhenCloud/FanqiePlugin"))?;
    for (label, value) in [("owner", owner), ("repo", repo)] {
        if value.is_empty() {
            return Err(anyhow!("安装源里的 {label} 不能为空"));
        }
        if !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err(anyhow!(
                "安装源里的 {label} 含非法字符: {value}（只允许字母、数字、- _ .）"
            ));
        }
        if value == "." || value == ".." {
            return Err(anyhow!("安装源里的 {label} 不能是 {value}"));
        }
    }
    Ok((owner.to_string(), repo.to_string()))
}

/// 插件名会变成文件名，必须白名单化：原版直接拿 name 拼路径，
/// 一个 `../../evil` 就能写到插件目录之外。
fn safe_plugin_name(name: &str) -> Result<&str> {
    if name.is_empty() {
        return Err(anyhow!("plugin.json 里的 name 不能为空"));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return Err(anyhow!(
            "plugin.json 里的 name 含非法字符: {name}（只允许字母、数字、- _）"
        ));
    }
    if name.len() > 64 {
        return Err(anyhow!("plugin.json 里的 name 过长: {} 字符", name.len()));
    }
    Ok(name)
}

/// 插件源站地址。抽成函数是为了能在测试里指向本地静态服务，
/// 否则"安装成功"这条路只能靠线上仓库碰运气才能验证到。
fn raw_base() -> String {
    std::env::var("ILP_RAW_BASE")
        .unwrap_or_else(|_| "https://raw.githubusercontent.com".to_string())
        .trim_end_matches('/')
        .to_string()
}

fn install(config: &Config, source: &str) -> Result<()> {
    let (owner, repo) = parse_source(source)?;
    let base = raw_base();

    // 主分支名不固定，main 拿不到就退回 master。
    let mut text = None;
    for branch in ["main", "master"] {
        let url = format!("{base}/{owner}/{repo}/refs/heads/{branch}/plugin.json");
        match crate::http::get_text(&url) {
            Ok(t) => {
                text = Some(t);
                break;
            }
            Err(e) => {
                tracing::debug!("{branch} 分支取 plugin.json 失败: {e:#}");
            }
        }
    }
    let text = text
        .ok_or_else(|| anyhow!("无法从 {owner}/{repo} 的 main 或 master 分支获取 plugin.json"))?;

    let pj: PluginJson = serde_json::from_str(&text).context("解析 plugin.json 失败")?;
    let name = safe_plugin_name(&pj.name)?.to_string();
    let url = pj
        .url
        .pick()
        .ok_or_else(|| anyhow!("plugin.json 里没有适用于当前平台的 url"))?;
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(anyhow!("plugin.json 里的 url 不是 http(s) 地址: {url}"));
    }

    let bytes = crate::http::get_bytes(url).with_context(|| format!("下载插件失败: {url}"))?;
    validate_wasm(&bytes)?;

    let dir = config.plugins_dir();
    fs::create_dir_all(&dir)?;
    let dest = dir.join(format!("{name}.wasm"));
    write_atomic(&dest, &bytes)?;
    println!("已安装插件 {name} 到 {}", dest.display());
    Ok(())
}

/// 校验下载到的确实是一段 WebAssembly 模块，而不是错误页或截断文件。
fn validate_wasm(bytes: &[u8]) -> Result<()> {
    const MAGIC: &[u8] = b"\0asm";
    if bytes.len() < 8 {
        return Err(anyhow!(
            "插件产物过小（{} 字节），可能下载不完整",
            bytes.len()
        ));
    }
    if !bytes.starts_with(MAGIC) {
        return Err(anyhow!(
            "插件产物不是 wasm（缺少 \\0asm 魔数），前 16 字节: {:02x?}",
            &bytes[..bytes.len().min(16)]
        ));
    }
    // 跳过魔数后的 4 字节是 版本/层 头，有两种合法形态：
    //   核心模块   01 00 00 00
    //   组件模型   0d 00 01 00
    //
    // 不能把它当 u32 小端比较：本项目的产物是组件模型，读出来是 65549，
    // 按"== 1"校验会把自家插件拒掉。
    match &bytes[4..8] {
        [1, 0, 0, 0] | [13, 0, 1, 0] => Ok(()),
        other => Err(anyhow!(
            "不认识的 wasm 头部 {:02x?}（期望核心模块 01 00 00 00 或组件 0d 00 01 00）",
            other
        )),
    }
}

fn write_atomic(dest: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = dest.with_extension("wasm.tmp");
    fs::write(&tmp, bytes)?;
    match fs::rename(&tmp, dest) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e.into())
        }
    }
}

fn uninstall(config: &Config, name: &str) -> Result<()> {
    let name = safe_plugin_name(name)?;
    let mut removed = false;
    for path in plugin_paths(config, name) {
        if path.exists() {
            fs::remove_file(&path)?;
            removed = true;
        }
    }
    // 顺手清掉该插件的 KV 存储目录。
    let data = config.plugins_data_dir().join(name);
    if data.exists() {
        fs::remove_dir_all(&data)?;
    }
    if removed {
        println!("已卸载插件 {name}");
        Ok(())
    } else {
        Err(anyhow!("插件不存在: {name}"))
    }
}

fn enable(config: &Config, name: &str) -> Result<()> {
    let name = safe_plugin_name(name)?;
    let disabled = config.disabled_plugins_dir().join(format!("{name}.wasm"));
    let enabled = config.plugins_dir().join(format!("{name}.wasm"));
    if !disabled.exists() {
        return Err(if enabled.exists() {
            anyhow!("插件 {name} 已经是启用状态")
        } else {
            anyhow!("未找到禁用插件: {name}")
        });
    }
    fs::create_dir_all(config.plugins_dir())?;
    fs::rename(&disabled, &enabled)?;
    println!("已启用插件 {name}");
    Ok(())
}

fn disable(config: &Config, name: &str) -> Result<()> {
    let name = safe_plugin_name(name)?;
    let enabled = config.plugins_dir().join(format!("{name}.wasm"));
    let disabled = config.disabled_plugins_dir().join(format!("{name}.wasm"));
    if !enabled.exists() {
        return Err(if disabled.exists() {
            anyhow!("插件 {name} 已经是禁用状态")
        } else {
            anyhow!("未找到启用插件: {name}")
        });
    }
    fs::create_dir_all(config.disabled_plugins_dir())?;
    fs::rename(&enabled, &disabled)?;
    println!("已禁用插件 {name}");
    Ok(())
}

fn plugin_paths(config: &Config, name: &str) -> Vec<PathBuf> {
    vec![
        config.plugins_dir().join(format!("{name}.wasm")),
        config.disabled_plugins_dir().join(format!("{name}.wasm")),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_accepts_normal_repo() {
        assert_eq!(
            parse_source("RhenCloud/FanqiePlugin").expect("应接受"),
            ("RhenCloud".to_string(), "FanqiePlugin".to_string())
        );
    }

    #[test]
    fn source_rejects_traversal_and_empty() {
        for bad in [
            "../etc/passwd",
            "owner/..",
            "a/b/c",
            "no-slash",
            "/repo",
            "owner/",
            "own er/repo",
            "owner/re po",
        ] {
            assert!(parse_source(bad).is_err(), "应拒绝: {bad}");
        }
    }

    #[test]
    fn plugin_name_whitelist_blocks_path_traversal() {
        // 原版直接拿 name 拼路径，`../evil` 会写到插件目录外。
        assert!(safe_plugin_name("fanqie").is_ok());
        assert!(safe_plugin_name("fan-qie_2").is_ok());
        for bad in ["", "../evil", "a/b", "a\\b", "a.b", "带中文", "a b"] {
            assert!(safe_plugin_name(bad).is_err(), "应拒绝: {bad:?}");
        }
        assert!(safe_plugin_name(&"a".repeat(65)).is_err());
    }

    #[test]
    fn wasm_validation_accepts_core_and_component() {
        // 核心模块
        assert!(validate_wasm(b"\0asm\x01\0\0\0").is_ok());
        // 组件模型：本项目自己编译出来的产物就是这个头，
        // 用"魔数后 u32 小端 == 1"来判会把它误拒。
        assert!(validate_wasm(b"\0asm\x0d\0\x01\0").is_ok());
    }

    #[test]
    fn wasm_validation_rejects_non_wasm() {
        // 常见失败：拿到的是 GitHub 的 404 页面
        assert!(validate_wasm(b"<!DOCTYPE html><html>").is_err());
        // 截断
        assert!(validate_wasm(b"\0asm").is_err());
        // 不认识的头
        assert!(validate_wasm(b"\0asm\x02\0\0\0").is_err());
    }

    #[test]
    fn real_built_plugin_passes_validation() {
        // 用仓库里真实构建出的插件产物做一次整体校验，
        // 防止"校验逻辑和实际产物头部不一致"这类问题再次漏过去。
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/wasm32-wasip2/release/ilp_plugin_fanqie.wasm");
        let Ok(bytes) = std::fs::read(&path) else {
            eprintln!("跳过：未找到 {}（先构建 wasm）", path.display());
            return;
        };
        assert!(
            validate_wasm(&bytes).is_ok(),
            "真实插件产物必须通过校验，头部 {:02x?}",
            &bytes[..8]
        );
    }

    #[test]
    fn url_field_supports_both_shapes() {
        let single: PluginJson =
            serde_json::from_str(r#"{"name":"a","url":"https://x/a.wasm"}"#).expect("单地址");
        assert_eq!(single.url.pick().expect("有地址"), "https://x/a.wasm");

        let by_os: PluginJson = serde_json::from_str(
            r#"{"name":"a","url":{"linux":"https://x/l.wasm","windows":"https://x/w.wasm"}}"#,
        )
        .expect("分平台");
        assert!(by_os.url.pick().is_some());

        // 官方模板用大写键，必须大小写不敏感。
        let caps: PluginJson = serde_json::from_str(
            r#"{"name":"a","url":{"Linux":"https://x/l.wasm","Windows":"https://x/w.wasm"}}"#,
        )
        .expect("大写键");
        let picked = caps.url.pick().expect("有地址");
        assert!(picked.ends_with(".wasm"), "拿到了 {picked}");
    }
}
