//! 诊断日志与诊断包。
//!
//! 为什么单独一层而不是"把 data 目录打个 zip"：诊断包会被贴到 issue 里，
//! 而 data 目录里有 manifest（含全部章节标题）、数据库副本、secret 文件。
//! 不做筛选的"打包上传"等于让用户在不知情的情况下公开自己的书单和凭据。
//!
//! 三条约束贯穿这个模块：
//!
//! 1. **秘密永不入包。** 不是"脱敏后入包"——secret 目录整个不碰，
//!    日志里出现的疑似凭据直接替换掉。
//! 2. **打包前展示清单。** 用户必须知道自己在分享什么，再决定要不要分享。
//! 3. **默认最小集。** 环境信息 + 脱敏日志 + manifest 摘要。想要更多得显式加。

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::config::Config;

/// 诊断包里单个文件的大小上限。
///
/// 日志能到几百 MB（一次失败的批量下载会疯狂重试）。截断而不是拒绝：
/// 拒绝打包等于在最需要诊断的时候不给工具。
const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

/// 保留的日志天数。
///
/// 限期保留是隐私要求的一部分：日志里必然有书名和章节标题，
/// 长期留在一个没人看的目录里只会变成泄漏面。
pub const LOG_RETENTION_DAYS: u64 = 7;

/// 日志里被判定为凭据的片段替换成它。
pub const REDACTED: &str = "<redacted>";

/// 疑似凭据的标记。按子串匹配，大小写不敏感。
///
/// 用子串而不是固定名单，是因为真实流量里总会有名单外的头
/// （`x-ms-token` 就是这么被发现的）。代价是可能误报——把一段普通文本
/// 替换成 `<redacted>` 无害，漏掉一个真实 token 有害。
const SECRET_MARKERS: &[&str] = &[
    "cookie",
    "authorization",
    "access_token",
    "refresh_token",
    "passport_csrf_token",
    "sessionid",
    "password",
    "passwd",
    "secret",
    "signing_key",
    "private_key",
    "x-ms-token",
    "x-tt-token",
    "bearer ",
];

/// 脱敏一行日志。
///
/// 策略：一旦某行命中标记，就把该行等号/冒号右侧的值换掉，保留左半边的
/// 键名。整行删掉会让日志失去"谁被脱敏了"的信息，调试时反而更麻烦。
pub fn redact_line(line: &str) -> String {
    let lower = line.to_ascii_lowercase();
    let hit = SECRET_MARKERS.iter().any(|m| lower.contains(m));
    if !hit {
        return line.to_string();
    }
    match split_value(line) {
        // 找不到分隔符（比如整行就是一段 base64），整行脱敏。
        None => format!("{REDACTED} (原因: 疑似凭据)"),
        // 分隔符已经在 key 里了（`Cookie:`、`access_token=`），
        // 再补一个 `=` 会变成 `access_token== <redacted>`。
        Some((key, _)) => format!("{key} {REDACTED}"),
    }
}

/// 找到「键名」与「值」的分界，返回（含分隔符的左半边，右半边）。
///
/// 不能直接在第一个 `=` 或 `:` 处切：日志行以时间戳开头
/// （`2026-10-31T10:00:01Z DEBUG Cookie: xxx`），在第一个冒号处切会把
/// 时间戳连同级别一起丢掉，脱敏后的行变成 `2026-10-31T10:= <redacted>`，
/// 完全失去诊断价值。
///
/// 正确的分界点是**命中标记的那个词**之后的分隔符。
fn split_value(line: &str) -> Option<(&str, &str)> {
    let lower = line.to_ascii_lowercase();
    // 找到最靠前的标记命中位置，从那里往后找分隔符。
    let marker_at = SECRET_MARKERS.iter().filter_map(|m| lower.find(m)).min()?;
    let rest = &line[marker_at..];
    let offset = rest.find(['=', ':'])?;
    let cut = marker_at + offset + 1;
    Some(line.split_at(cut))
}

/// 脱敏整段文本，逐行处理。
pub fn redact(text: &str) -> String {
    text.lines().map(redact_line).collect::<Vec<_>>().join("\n")
}

/// 诊断包里的一项。清单展示和实际打包用同一份数据，
/// 避免"展示的清单"和"真实内容"不一致——那种不一致比不展示更糟。
pub struct Entry {
    /// 包内路径
    pub name: String,
    /// 内容
    pub body: Vec<u8>,
    /// 清单里给人看的说明
    pub note: String,
}

impl Entry {
    fn text(name: &str, body: String, note: &str) -> Self {
        Self {
            name: name.to_string(),
            body: body.into_bytes(),
            note: note.to_string(),
        }
    }
}

/// 收集诊断信息。
///
/// `include_manifest` 由 `--with-manifest` 控制：manifest 里有书名和完整
/// 章节标题，能定位"哪一章解析错了"，但也是用户的阅读记录。默认不含。
pub fn collect(config: &Config, include_manifest: bool) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();

    entries.push(Entry::text(
        "environment.txt",
        environment(config),
        "程序版本、操作系统、架构；不含路径与用户名",
    ));

    entries.push(Entry::text(
        "config-summary.txt",
        config_summary(config),
        "生效的配置项；密钥类字段只显示数量",
    ));

    entries.push(Entry::text(
        "plugins.txt",
        plugins_summary(config),
        "已安装插件的名称与大小；不读取插件内部数据",
    ));

    let logs = collect_logs(config, &mut entries)?;
    if !logs.is_empty() {
        entries.push(Entry::text(
            "logs/ilp.log",
            logs,
            "最近的运行日志（已脱敏、已截断）",
        ));
    }

    if include_manifest {
        for (name, body) in manifest_summaries(config)? {
            entries.push(Entry::text(
                &format!("manifests/{name}"),
                body,
                "manifest 摘要（含书名与章节标题）",
            ));
        }
    }

    Ok(entries)
}

fn environment(config: &Config) -> String {
    let mut out = String::new();
    out.push_str(&format!("ilp 版本: {}\n", env!("CARGO_PKG_VERSION")));
    out.push_str(&format!("ABI: {}\n", crate::plugin_manager::CURRENT_ABI));
    out.push_str(&format!("操作系统: {}\n", std::env::consts::OS));
    out.push_str(&format!("架构: {}\n", std::env::consts::ARCH));
    // 路径本身可能含用户名（/home/alice/...），只报存在性。
    out.push_str(&format!(
        "数据目录存在: {}\n",
        Path::new(&config.data_folder).exists()
    ));
    out
}

fn config_summary(config: &Config) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "max_workers: {}\n",
        config.effective_max_workers()
    ));
    out.push_str(&format!("sleep_time: {} 秒\n", config.sleep_time));
    out.push_str(&format!(
        "proxy: {}\n",
        if config.proxy.is_some() {
            "已设置"
        } else {
            "未设置"
        }
    ));
    out.push_str(&format!("no_proxy 条数: {}\n", config.no_proxy.len()));
    out.push_str(&format!("log_level: {}\n", config.log_level));
    out.push_str(&format!("log_to_stderr: {}\n", config.log_to_stderr));
    // 公钥不是秘密，但没必要进包；用户要分享的是"有没有配"。
    out.push_str(&format!(
        "plugin_trusted_keys: {} 个\n",
        config.plugin_trusted_keys.len()
    ));
    out.push_str(&format!(
        "plugin_revoked_keys: {} 个\n",
        config.plugin_revoked_keys.len()
    ));
    out.push_str(&format!(
        "require_plugin_signatures: {}\n",
        config.require_plugin_signatures
    ));
    out.push_str(&format!(
        "require_registry_signatures: {}\n",
        config.require_registry_signatures
    ));
    out
}

fn plugins_summary(config: &Config) -> String {
    let mut out = String::new();
    for (label, dir) in [
        ("已启用", config.plugins_dir()),
        ("已禁用", config.disabled_plugins_dir()),
    ] {
        let names = list_wasm(&dir);
        if names.is_empty() {
            continue;
        }
        out.push_str(&format!("{label}:\n"));
        for name in names {
            let size = std::fs::metadata(dir.join(format!("{name}.wasm")))
                .map(|m| m.len())
                .unwrap_or(0);
            out.push_str(&format!("  {name} ({size} 字节)\n"));
        }
    }
    if out.is_empty() {
        out.push_str("没有已安装的插件\n");
    }
    out
}

/// 判断是不是日志文件。
///
/// `tracing_appender` 的按日归档名字是 `ilp.log.YYYY-MM-DD`，扩展名部分
/// 是日期而不是 `log`，所以不能用 `extension()` 判断。
fn is_log_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("ilp.log"))
}

/// 列出目录下的 `.wasm` 文件名（不含扩展名）。
fn list_wasm(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path();
            if path.extension()?.to_str()? != "wasm" {
                return None;
            }
            Some(path.file_stem()?.to_str()?.to_string())
        })
        .collect();
    names.sort();
    names
}

/// 读取最近的日志并脱敏。
///
/// 从最新的往旧的读，累计到上限就停。只读不删——删日志是 `prune_logs`
/// 的事，诊断包不该有副作用。
fn collect_logs(config: &Config, entries: &mut Vec<Entry>) -> Result<String> {
    let dir = config.logs_dir();
    let Ok(read) = std::fs::read_dir(&dir) else {
        return Ok(String::new());
    };
    let mut files: Vec<PathBuf> = read
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        // 文件名是 `ilp.log.2026-10-30`，`extension()` 会返回 `2026-10-30`
        // 而不是 `log`。按扩展名判断会一个日志都收不到——诊断包看起来
        // "生成成功"，实际没有任何日志内容，这才是最坏的情况。
        .filter(|p| is_log_file(p))
        .collect();
    // 文件名带日期，倒序即最新在前。
    files.sort();
    files.reverse();

    let mut out = String::new();
    let mut remaining = MAX_LOG_BYTES;
    let mut truncated = false;
    for path in files {
        if remaining == 0 {
            truncated = true;
            break;
        }
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        let take = meta.len().min(remaining);
        let Ok(content) = read_head(&path, take) else {
            continue;
        };
        out.push_str(&format!(
            "# {}\n",
            path.file_name().unwrap_or_default().to_string_lossy()
        ));
        out.push_str(&redact(&content));
        if !out.ends_with('\n') {
            out.push('\n');
        }
        remaining -= take;
        if meta.len() > take {
            truncated = true;
        }
    }
    if truncated {
        entries.push(Entry::text(
            "logs/TRUNCATED.txt",
            format!("日志超过 {MAX_LOG_BYTES} 字节，只保留了最近的部分\n"),
            "日志截断说明",
        ));
    }
    Ok(out)
}

/// 读文件开头 `limit` 字节。用 `take` 而不是整个读进来再截断：
/// 几百 MB 的日志整个读进内存会直接 OOM。
fn read_head(path: &Path, limit: u64) -> Result<String> {
    use std::io::Read;
    let file =
        std::fs::File::open(path).with_context(|| format!("打开日志失败: {}", path.display()))?;
    let mut buf = Vec::new();
    file.take(limit).read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// 生成 manifest 摘要：每本书的章节数、状态分布，**不含正文**。
fn manifest_summaries(config: &Config) -> Result<Vec<(String, String)>> {
    let dir = config.novels_dir();
    let Ok(read) = std::fs::read_dir(&dir) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for entry in read.filter_map(|e| e.ok()) {
        let path = entry.path();
        let is_manifest = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(".manifest.json"));
        if !is_manifest {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        out.push((
            path.file_name().unwrap().to_string_lossy().into_owned(),
            manifest_summary(&value),
        ));
    }
    Ok(out)
}

/// 把 manifest 压成"多少章、各状态几章"。
fn manifest_summary(value: &serde_json::Value) -> String {
    let mut out = String::new();
    if let Some(title) = value.get("title").and_then(|v| v.as_str()) {
        out.push_str(&format!("书名: {title}\n"));
    }
    let chapters = value
        .get("chapters")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    out.push_str(&format!("章节数: {}\n", chapters.len()));
    let mut by_status: BTreeMap<String, usize> = BTreeMap::new();
    for chapter in chapters.values() {
        let status = chapter
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        *by_status.entry(status.to_string()).or_insert(0) += 1;
    }
    out.push_str("状态分布:\n");
    for (status, count) in by_status {
        out.push_str(&format!("  {status}: {count}\n"));
    }
    out
}

/// 删除超过保留期的日志。
///
/// 返回删掉的文件数。失败不致命：日志清不掉不该阻断用户干活。
pub fn prune_logs(config: &Config) -> usize {
    let dir = config.logs_dir();
    let Ok(read) = std::fs::read_dir(&dir) else {
        return 0;
    };
    let cutoff = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(LOG_RETENTION_DAYS * 86400));
    let Some(cutoff) = cutoff else {
        return 0;
    };
    let mut removed = 0;
    for entry in read.filter_map(|e| e.ok()) {
        let path = entry.path();
        if !is_log_file(&path) {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        if modified < cutoff && std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// 把条目打成 tar.gz。
///
/// 自己写 tar 而不是引入依赖：只需要"一串文件打成 tar"这一件事，
/// tar 格式的这一小部分足够简单且几十年不变。
pub fn write_bundle(path: &Path, entries: &[Entry]) -> Result<()> {
    let file = std::fs::File::create(path)
        .with_context(|| format!("创建诊断包失败: {}", path.display()))?;
    let mut enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    for entry in entries {
        write_tar_entry(&mut enc, &entry.name, &entry.body)?;
    }
    // tar 以两个全零块结束。
    enc.write_all(&[0u8; 1024])?;
    enc.finish()?.sync_all()?;
    Ok(())
}

fn write_tar_entry<W: Write>(out: &mut W, name: &str, body: &[u8]) -> Result<()> {
    let mut header = [0u8; 512];
    write_bytes(&mut header[0..100], name.as_bytes())?;
    write_octal(&mut header[100..108], 0o644)?; // mode
    write_octal(&mut header[108..116], 0)?; // uid
    write_octal(&mut header[116..124], 0)?; // gid
    write_octal(&mut header[124..136], body.len() as u64)?; // size
    write_octal(&mut header[136..148], 0)?; // mtime：固定为 0，让同样的输入产出同样的包
    header[156] = b'0'; // 普通文件
    write_bytes(&mut header[148..156], b"        ")?; // 校验和先用空格占位
    let checksum: u32 = header.iter().map(|b| *b as u32).sum();
    write_checksum(&mut header[148..156], checksum)?;
    out.write_all(&header)?;
    out.write_all(body)?;
    // tar 要求每项补齐到 512 字节边界。
    let pad = (512 - body.len() % 512) % 512;
    if pad > 0 {
        out.write_all(&vec![0u8; pad])?;
    }
    Ok(())
}

fn write_bytes(dst: &mut [u8], src: &[u8]) -> Result<()> {
    if src.len() > dst.len() {
        anyhow::bail!("tar 字段溢出: {} > {}", src.len(), dst.len());
    }
    dst[..src.len()].copy_from_slice(src);
    Ok(())
}

fn write_octal(dst: &mut [u8], value: u64) -> Result<()> {
    // 8 字节字段：7 位八进制数字 + NUL。
    let text = format!("{:0width$o}", value, width = dst.len() - 1);
    if text.len() > dst.len() - 1 {
        anyhow::bail!("tar 数值字段溢出: {value}");
    }
    write_bytes(dst, text.as_bytes())?;
    dst[dst.len() - 1] = 0;
    Ok(())
}

fn write_checksum(dst: &mut [u8], value: u32) -> Result<()> {
    let text = format!("{value:06o}");
    write_bytes(dst, text.as_bytes())?;
    dst[6] = 0;
    dst[7] = b' ';
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_keeps_the_key_and_drops_the_value() {
        // 保留键名：调试时需要知道"哪个字段被脱敏了"。
        let out = redact_line("Cookie: sessionid=abc123def456");
        assert!(out.contains(REDACTED), "实得: {out}");
        assert!(!out.contains("abc123def456"), "真实值泄漏: {out}");
    }

    #[test]
    fn redaction_preserves_the_timestamp_and_level() {
        // 真实日志行以时间戳开头，里面有一堆冒号。脱敏后必须仍能看出
        // "什么时候、什么级别、哪个字段被脱敏"，否则日志就没用了。
        let out = redact_line("2026-10-31T10:00:01Z DEBUG Cookie: sessionid=deadbeef");
        assert!(
            out.starts_with("2026-10-31T10:00:01Z DEBUG"),
            "前缀被吃掉: {out}"
        );
        assert!(out.contains("Cookie"), "键名丢了: {out}");
        assert!(out.contains(REDACTED));
        assert!(!out.contains("deadbeef"), "真实值泄漏: {out}");
    }

    #[test]
    fn redaction_is_case_insensitive_and_covers_unlisted_headers() {
        // x-ms-token 曾经逃过固定名单，子串匹配就是为它改的。
        for line in [
            "AUTHORIZATION: Bearer abcdefghijklmnop",
            "x-ms-token: deadbeefcafe",
            "X-TT-Token: 1234567890abcdef",
            "access_token=verysecretvalue",
        ] {
            let out = redact_line(line);
            assert!(out.contains(REDACTED), "{line} 未被脱敏，实得: {out}");
        }
    }

    #[test]
    fn ordinary_lines_pass_through_untouched() {
        // 过度脱敏会让日志失去诊断价值。
        for line in [
            "INFO 已下载: 某书 - 第1章",
            "已加载插件: fanqie 371899 字节",
            "配置已加载 max_workers=1",
        ] {
            assert_eq!(redact_line(line), line, "不该改动: {line}");
        }
    }

    #[test]
    fn secret_without_separator_is_fully_redacted() {
        // 整行就是一段裸 token 时，没有"键名"可保留。
        let out = redact_line("cookie abcdefghijklmnop");
        assert!(out.contains(REDACTED));
        assert!(!out.contains("abcdefghijklmnop"));
    }

    #[test]
    fn environment_never_leaks_the_data_path() {
        let config = Config::default();
        let text = environment(&config);
        // 路径可能含用户名，只报存在性。
        assert!(!text.contains(&config.data_folder) || config.data_folder == "data");
        assert!(text.contains("操作系统"));
    }

    #[test]
    fn config_summary_counts_keys_without_printing_them() {
        let mut config = Config::default();
        config
            .plugin_trusted_keys
            .insert("release".into(), "AAAA".into());
        let text = config_summary(&config);
        assert!(text.contains("plugin_trusted_keys: 1 个"), "实得: {text}");
        assert!(!text.contains("AAAA"), "公钥值不该进包");
    }

    #[test]
    fn manifest_summary_counts_statuses_not_bodies() {
        let value = serde_json::json!({
            "title": "某书",
            "chapters": {
                "1": {"status": "ok", "content": "正文不该出现"},
                "2": {"status": "ok"},
                "3": {"status": "failed"}
            }
        });
        let text = manifest_summary(&value);
        assert!(text.contains("章节数: 3"), "实得: {text}");
        assert!(text.contains("ok: 2"), "实得: {text}");
        assert!(text.contains("failed: 1"), "实得: {text}");
        assert!(!text.contains("正文不该出现"), "正文泄漏进摘要: {text}");
    }

    #[test]
    fn expired_logs_are_pruned_but_recent_ones_survive() {
        // 限期保留是隐私要求的一部分，不是"磁盘清理"这种可选项。
        let dir = std::env::temp_dir().join(format!("ilp-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut config = Config::default();
        config.data_folder = dir.to_string_lossy().into_owned();

        let logs = config.logs_dir();
        std::fs::create_dir_all(&logs).unwrap();
        // 名字要符合 tracing_appender 的归档格式。
        let old = logs.join("ilp.log.2020-01-01");
        let new = logs.join("ilp.log.2026-10-31");
        std::fs::write(&old, "旧日志").unwrap();
        std::fs::write(&new, "新日志").unwrap();
        // 判断依据是 mtime 而不是文件名里的日期——文件名可以改，mtime 才是
        // 文件真实老了多久。刚写的文件 mtime 是"现在"，所以要把旧的往回拨。
        set_mtime_days_ago(&old, LOG_RETENTION_DAYS + 1);

        let removed = prune_logs(&config);
        assert_eq!(removed, 1, "应只删掉过期的那一个");
        assert!(!old.exists(), "过期日志还在");
        assert!(new.exists(), "新日志被误删");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 把文件 mtime 往回拨 `days` 天。用 `utimensat` 不方便，直接调系统命令。
    fn set_mtime_days_ago(path: &Path, days: u64) {
        let stamp = format!("-{days} days");
        let status = std::process::Command::new("touch")
            .arg("-d")
            .arg(&stamp)
            .arg(path)
            .status()
            .expect("调用 touch");
        assert!(status.success(), "touch 失败");
    }

    #[test]
    fn log_files_are_recognised_by_prefix_not_extension() {
        // ilp.log.2026-10-30 的 extension() 是 "2026-10-30" 而不是 "log"。
        // 按扩展名判断会一个日志都收不到，诊断包看着生成成功实际是空的。
        assert!(is_log_file(Path::new("ilp.log.2026-10-30")));
        assert!(is_log_file(Path::new("ilp.log")));
        assert!(!is_log_file(Path::new("config.json")));
        assert!(!is_log_file(Path::new("something.log")));
    }

    #[test]
    fn bundle_is_a_valid_gzipped_tar() {
        let dir = std::env::temp_dir().join(format!("ilp-bundle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("diag.tar.gz");
        let entries = vec![
            Entry::text("a.txt", "hello\n".into(), "测试"),
            Entry::text("dir/b.txt", "world\n".into(), "测试"),
        ];
        write_bundle(&path, &entries).expect("打包");

        // gzip 魔数
        let raw = std::fs::read(&path).unwrap();
        assert_eq!(&raw[..2], &[0x1f, 0x8b], "不是 gzip");

        // 解压后检查 tar 结构：每个文件名都该出现。
        use std::io::Read;
        let mut dec = flate2::read::GzDecoder::new(&raw[..]);
        let mut tar = Vec::new();
        dec.read_to_end(&mut tar).expect("解压");
        let text = String::from_utf8_lossy(&tar);
        assert!(text.contains("a.txt"));
        assert!(text.contains("dir/b.txt"));
        assert!(text.contains("hello"));
        assert!(text.contains("world"));
        // 长度必须是 512 的整数倍（末尾两块零）。
        assert_eq!(tar.len() % 512, 0, "tar 未对齐到 512 字节");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
