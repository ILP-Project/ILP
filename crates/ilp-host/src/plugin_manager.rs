use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use sha2::Digest;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::cli::PluginCommand;
use crate::config::Config;
use crate::signing::SignatureSet;

/// 站点只发布一个 ABI 版本。必须与 `ilp-plugin-wit` 里
/// `package ilp:plugin@x.y.z` 保持一致。
pub const CURRENT_ABI: &str = "0.2.0";
pub const SUPPORTED_ABIS: &[&str] = &["0.1.0", CURRENT_ABI];
/// 保留旧名称，避免外部集成在迁移期间中断。
#[allow(dead_code)]
pub const SUPPORTED_ABI: &str = CURRENT_ABI;

fn is_supported_abi(abi: &str) -> bool {
    SUPPORTED_ABIS.contains(&abi)
}

/// 插件 registry（`registry.json`）。
///
/// 只取用得到的字段：宿主不该因为 registry 多了个新字段就装不了插件。
#[derive(Deserialize)]
struct Registry {
    #[serde(default)]
    schema: u32,
    plugins: Vec<RegistryPlugin>,
}

#[derive(Deserialize, Debug, Clone)]
struct RegistryPlugin {
    name: String,
    site_id: String,
    abi: String,
    latest: String,
    versions: Vec<String>,
    url: String,
    /// registry 声明的产物大小与摘要，用于拒绝截断/被改的下载。
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    sha256: Option<String>,
    /// 每个版本的独立产物信息。旧 schema 没有此字段，回退到顶层字段。
    #[serde(default)]
    artifacts: HashMap<String, RegistryArtifact>,
    #[serde(default)]
    signatures: HashMap<String, RegistrySignatures>,
}

/// 每版本签名集合，与 registry 元数据签名共用同一套信任语义。
type RegistrySignatures = SignatureSet;

#[derive(Deserialize, Debug, Clone)]
struct RegistryArtifact {
    url: String,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    sha256: Option<String>,
}

impl Registry {
    /// registry 自己声明的 site_id 是否重复。
    ///
    /// 宿主按 site_id 找插件，重复会让行为取决于文件加载顺序
    /// （`load_plugins` 里对文件名排序）。registry 是最适合做这项校验的地方：
    /// 它能看到全部插件，而单个插件自己看不到。
    fn duplicate_site_ids(&self) -> Vec<(&str, Vec<&str>)> {
        let mut by_site: std::collections::BTreeMap<&str, Vec<&str>> =
            std::collections::BTreeMap::new();
        for p in &self.plugins {
            by_site
                .entry(p.site_id.as_str())
                .or_default()
                .push(p.name.as_str());
        }
        by_site
            .into_iter()
            .filter(|(_, names)| names.len() > 1)
            .collect()
    }

    fn find(&self, name: &str) -> Result<&RegistryPlugin> {
        self.plugins.iter().find(|p| p.name == name).ok_or_else(|| {
            let mut names: Vec<&str> = self.plugins.iter().map(|p| p.name.as_str()).collect();
            names.sort();
            if names.is_empty() {
                anyhow!("registry 里没有任何插件")
            } else {
                anyhow!("没有名为 {name} 的插件。可用插件: {}", names.join(", "))
            }
        })
    }
}

pub fn run(config: &Config, cmd: PluginCommand) -> Result<()> {
    match cmd {
        PluginCommand::List => list(config),
        PluginCommand::Info { name } => info(config, &name),
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
        println!("没有已安装的插件。用 `ilp plugin install <name>` 安装。");
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

fn info(config: &Config, name: &str) -> Result<()> {
    let name = safe_plugin_name(name)?;
    let path = config.plugins_dir().join(format!("{name}.wasm"));
    if !path.exists() {
        return Err(anyhow!("未找到已启用插件: {name}"));
    }
    let bytes = fs::read(&path)?;
    verify_artifact(
        &bytes,
        &RegistryPlugin {
            name: name.to_string(),
            site_id: String::new(),
            abi: String::new(),
            latest: String::new(),
            versions: Vec::new(),
            url: String::new(),
            size: Some(bytes.len() as u64),
            sha256: Some(format!("{:x}", sha2::Sha256::digest(&bytes))),
            artifacts: HashMap::new(),
            signatures: HashMap::new(),
        },
    )?;
    let digest = format!("{:x}", sha2::Sha256::digest(&bytes));
    println!(
        "插件: {name}\n文件: {}\n大小: {} 字节\nSHA-256: {digest}",
        path.display(),
        bytes.len()
    );
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

/// 解析 `name[@version]`。两段都会进 URL 和文件名，必须白名单化。
fn parse_source(source: &str) -> Result<(String, String)> {
    let (name, version) = match source.split_once('@') {
        Some((n, v)) => (n, v),
        None => (source, "latest"),
    };
    for (label, value) in [("插件名", name), ("版本", version)] {
        if value.is_empty() {
            return Err(anyhow!("{label}不能为空"));
        }
        if !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err(anyhow!(
                "{label}含非法字符: {value}（只允许字母、数字、- _ .）"
            ));
        }
        if value == "." || value == ".." {
            return Err(anyhow!("{label}不能是 {value}"));
        }
    }
    Ok((name.to_string(), version.to_string()))
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

/// registry 的默认地址。发布站点由 plugins 仓库的 `publish.yml` 生成，
/// 托管在 Cloudflare Pages 上，`ilp.elysola.org` 是其自定义域名。
pub const DEFAULT_REGISTRY: &str = "https://ilp.elysola.org";

/// registry 地址。抽成函数是为了能在测试里指向本地静态服务，
/// 否则"安装成功"这条路只能靠线上站点碰运气才能验证到。
fn registry_base() -> String {
    std::env::var("ILP_REGISTRY_URL")
        .unwrap_or_else(|_| DEFAULT_REGISTRY.to_string())
        .trim_end_matches('/')
        .to_string()
}

/// 把 registry 里的相对 url 补成绝对地址。
///
/// registry 存相对路径（`fanqie/1.0.0/fanqie.wasm`），这样换域名不用重发 registry；
/// 但 CI 若给了绝对地址也得能用，所以只对相对路径拼接。
fn resolve_url(base: &str, url: &str) -> Result<String> {
    let full = if url.starts_with("http://") || url.starts_with("https://") {
        url.to_string()
    } else if url.starts_with('/') || url.contains("..") {
        return Err(anyhow!("registry 里的 url 非法: {url}"));
    } else {
        format!("{base}/{url}")
    };
    Ok(full)
}

/// 校验下载产物与 registry 声明一致。
///
/// 没有这一步时，截断的下载只要前 8 字节对就能装进去，然后在运行时以
/// 一个和真实原因毫无关系的 wasmtime 错误失败。
fn clone_registry_plugin(p: &RegistryPlugin) -> RegistryPlugin {
    RegistryPlugin {
        name: p.name.clone(),
        site_id: p.site_id.clone(),
        abi: p.abi.clone(),
        latest: p.latest.clone(),
        versions: p.versions.clone(),
        url: p.url.clone(),
        size: p.size,
        sha256: p.sha256.clone(),
        artifacts: p.artifacts.clone(),
        signatures: p.signatures.clone(),
    }
}

fn verify_signature(
    config: &Config,
    plugin: &RegistryPlugin,
    version: &str,
    bytes: &[u8],
) -> Result<()> {
    let subject = format!("插件 {} {}", plugin.name, version);
    let Some(signatures) = plugin.signatures.get(version) else {
        if config.require_plugin_signatures {
            return Err(anyhow!("{subject} 缺少签名"));
        }
        tracing::warn!(plugin = %plugin.name, version, "插件没有签名，继续安装（未启用强制签名校验）");
        return Ok(());
    };
    crate::signing::verify(config, &subject, bytes, signatures)
}

fn verify_artifact(bytes: &[u8], p: &RegistryPlugin) -> Result<()> {
    if let Some(size) = p.size {
        if bytes.len() as u64 != size {
            return Err(anyhow!(
                "产物大小不符: registry 声明 {size} 字节，实得 {} 字节",
                bytes.len()
            ));
        }
    }
    if let Some(want) = &p.sha256 {
        use sha2::{Digest, Sha256};
        let got = format!("{:x}", Sha256::digest(bytes));
        if !got.eq_ignore_ascii_case(want) {
            return Err(anyhow!("产物校验和不符: registry 声明 {want}，实得 {got}"));
        }
    }
    validate_wasm(bytes)
}

fn install(config: &Config, source: &str) -> Result<()> {
    let (name, version) = parse_source(source)?;
    // parse_source 的白名单比文件名要求宽松（允许 `.`，为了版本号），
    // 而 name 会直接拼成 `<name>.wasm`。再过一遍文件名白名单，
    // 保证两条路径用的是同一个标准。
    let name = safe_plugin_name(&name)?.to_string();
    let base = registry_base();

    let registry_url = format!("{base}/registry.json");
    let raw = crate::http::get_bytes(&registry_url)
        .context("获取插件 registry 失败（可用 ILP_REGISTRY_URL 指定镜像）")?;
    verify_registry_signature(config, &base, &raw)?;
    let text = String::from_utf8(raw).context("registry.json 不是有效 UTF-8")?;
    let reg: Registry = serde_json::from_str(&text).context("解析 registry.json 失败")?;
    if reg.schema != 0 && reg.schema != 1 {
        return Err(anyhow!(
            "registry schema {} 不受支持（本版本支持 1），请升级 ilp",
            reg.schema
        ));
    }

    // site_id 重复时先告警而不是拒绝：装插件这个动作本身没错，
    // 但不说明白的话，用户会看到"装了两个同站点插件，只有其中一个生效"。
    for (site_id, names) in reg.duplicate_site_ids() {
        tracing::warn!(
            "registry 里 site_id {site_id} 被多个插件声明: {}。按 site_id 查找时行为不确定",
            names.join(", ")
        );
    }

    let p = reg.find(&name)?;

    // ABI 在下载前就比对：拿一个注定跑不起来的插件下来毫无意义，
    // 而且装进去之后只会在某次调用里以无关错误的形式暴露。
    if !is_supported_abi(&p.abi) {
        return Err(anyhow!(
            "插件 {name} 要求 ABI {}，本版本 ilp 支持 {}。请升级 ilp 或安装兼容版本",
            p.abi,
            SUPPORTED_ABIS.join(", ")
        ));
    }
    if p.abi != CURRENT_ABI {
        tracing::warn!(
            "插件 {name} 使用已弃用 ABI {}；当前 ABI 为 {CURRENT_ABI}，请尽快升级插件",
            p.abi
        );
    }

    let want = if version == "latest" {
        p.latest.clone()
    } else {
        version
    };
    if !p.versions.iter().any(|v| v == &want) {
        let mut vs = p.versions.clone();
        vs.sort();
        return Err(anyhow!(
            "插件 {name} 没有版本 {want}。可用版本: {}",
            vs.join(", ")
        ));
    }

    let artifact = p.artifacts.get(&want);
    let artifact_url = artifact.map(|a| a.url.as_str()).unwrap_or(&p.url);
    let url = resolve_url(&base, artifact_url)?;
    let bytes = crate::http::get_bytes(&url).with_context(|| format!("下载插件失败: {url}"))?;
    verify_signature(config, p, &want, &bytes)?;
    let expected = RegistryPlugin {
        url: artifact
            .map(|a| a.url.clone())
            .unwrap_or_else(|| p.url.clone()),
        size: artifact.and_then(|a| a.size).or(p.size),
        sha256: artifact
            .and_then(|a| a.sha256.clone())
            .or_else(|| p.sha256.clone()),
        ..clone_registry_plugin(p)
    };
    verify_artifact(&bytes, &expected)?;

    let dir = config.plugins_dir();
    fs::create_dir_all(&dir)?;
    let dest = dir.join(format!("{name}.wasm"));
    write_atomic(&dest, &bytes)?;
    println!("已安装插件 {name} {want} 到 {}", dest.display());
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

/// 校验 registry 元数据的签名。
///
/// 插件产物本身已单独签名，为什么还要签元数据？因为产物签名只证明
/// "这段代码是某个可信发布者给的"，不证明"这个名字和版本该指向它"。
/// 元数据未签名时，攻击者可以把 `install fanqie` 指到一个有效签名但
/// 有已知漏洞的旧版本，或者把名字重定向到别的插件。
///
/// 签名是对 `registry.json` 的**原始字节**，放在旁路的 `registry.json.sig`：
/// 把签名写进 JSON 内部就成了自指（签名内容含签名），而重新序列化做
/// 规范化又会引入"两侧算法必须完全一致"的隐式契约。
fn verify_registry_signature(config: &Config, base: &str, bytes: &[u8]) -> Result<()> {
    let sig_url = format!("{base}/registry.json.sig");
    let Some(raw) = crate::http::get_bytes_if_exists(&sig_url)? else {
        if config.require_registry_signatures {
            return Err(anyhow!(
                "registry 元数据没有签名（{sig_url} 不存在），已启用 require_registry_signatures。\
                 请把镜像升级到带签名的版本，或关闭该开关"
            ));
        }
        tracing::warn!("registry 元数据没有签名，跳过校验（未启用 require_registry_signatures）");
        return Ok(());
    };
    let set: SignatureSet =
        serde_json::from_slice(&raw).with_context(|| format!("解析 {sig_url} 失败"))?;
    crate::signing::verify(config, "registry 元数据", bytes, &set)
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
    use crate::signing::SignatureEntry;
    use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};

    #[test]
    fn source_accepts_name_and_optional_version() {
        assert_eq!(
            parse_source("fanqie").expect("应接受"),
            ("fanqie".to_string(), "latest".to_string())
        );
        assert_eq!(
            parse_source("fanqie@1.2.0").expect("应接受"),
            ("fanqie".to_string(), "1.2.0".to_string())
        );
        assert_eq!(
            parse_source("my-plugin_2@0.1.0").expect("应接受"),
            ("my-plugin_2".to_string(), "0.1.0".to_string())
        );
    }

    #[test]
    fn source_rejects_traversal_and_empty() {
        for bad in [
            "",
            "@1.0.0",
            "fanqie@",
            "../etc/passwd",
            "fanqie@..",
            "a/b",
            "own er",
            "fanqie@1.0.0 beta",
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
        // 常见失败：拿到的是错误页
        assert!(validate_wasm(b"<!DOCTYPE html><html>").is_err());
        // 截断
        assert!(validate_wasm(b"\0asm").is_err());
        // 不认识的头
        assert!(validate_wasm(b"\0asm\x02\0\0\0").is_err());
    }

    #[test]
    fn real_built_plugin_passes_validation() {
        // 用真实构建出的插件产物做一次整体校验，
        // 防止"校验逻辑和实际产物头部不一致"这类问题再次漏过去。
        // 插件已迁到兄弟仓库，优先看那边的产物。
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let candidates = [
            root.join("../plugins/target/wasm32-wasip2/release/ilp_plugin_fanqie.wasm"),
            root.join("target/wasm32-wasip2/release/ilp_plugin_fanqie.wasm"),
        ];
        let Some(path) = candidates.iter().find(|p| p.exists()) else {
            eprintln!("跳过：未找到 wasm 产物（先构建 wasm）");
            return;
        };
        let bytes = std::fs::read(path).expect("读取 wasm");
        assert!(
            validate_wasm(&bytes).is_ok(),
            "真实插件产物必须通过校验，头部 {:02x?}",
            &bytes[..8]
        );
    }

    fn sample_registry() -> Registry {
        serde_json::from_str(
            r#"{
                "schema": 1,
                "plugins": [{
                    "name": "fanqie",
                    "site_id": "fanqie",
                    "abi": "0.1.0",
                    "latest": "1.2.0",
                    "versions": ["1.2.0", "1.1.0"],
                    "url": "fanqie/1.2.0/fanqie.wasm"
                }]
            }"#,
        )
        .expect("样例 registry")
    }

    /// registry 元数据签名要能自动跟随安装路径校验；这里直接测
    /// 校验函数本身，避免为它起一个 HTTP 服务。
    #[test]
    fn registry_metadata_signature_is_verified() {
        use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
        use ed25519_dalek::{Signer, SigningKey};

        let key = SigningKey::from_bytes(&[41u8; 32]);
        let body = br#"{"schema":1,"plugins":[]}"#;
        let mut config = Config::default();
        config.plugin_trusted_keys = [(
            "meta".to_string(),
            B64.encode(key.verifying_key().to_bytes()),
        )]
        .into_iter()
        .collect();
        config.require_plugin_signatures = true;

        let good = SignatureSet::One(SignatureEntry {
            key_id: "meta".into(),
            signature: B64.encode(key.sign(body).to_bytes()),
        });
        crate::signing::verify(&config, "registry 元数据", body, &good).expect("签名应通过");

        // 元数据被改一位就必须拒绝：这正是“元数据未签名能被重定向”的防线。
        let mut tampered = body.to_vec();
        tampered[10] ^= 0xff;
        let err = crate::signing::verify(&config, "registry 元数据", &tampered, &good)
            .expect_err("篡改必须拒绝");
        assert!(format!("{err:#}").contains("签名校验失败"), "实得: {err:#}");
    }

    #[test]
    fn registry_lookup_reports_available_names() {
        // 装错名字是常见操作，错误信息要给出可选名字，
        // 否则用户只能去翻网页。
        let reg = sample_registry();
        let err = format!("{:#}", reg.find("nope").expect_err("不存在"));
        assert!(err.contains("fanqie"), "应列出可用插件，实得: {err}");
    }

    #[test]
    fn duplicate_site_ids_are_detected() {
        // 宿主按 site_id 找插件，重复时行为取决于文件名排序。
        // registry 是唯一能看到全局的地方，必须在这里发现。
        let reg: Registry = serde_json::from_str(
            r#"{
                "schema": 1,
                "plugins": [
                    {"name":"a","site_id":"x","abi":"0.1.0","latest":"1","versions":["1"],"url":"a.wasm"},
                    {"name":"b","site_id":"x","abi":"0.1.0","latest":"1","versions":["1"],"url":"b.wasm"},
                    {"name":"c","site_id":"y","abi":"0.1.0","latest":"1","versions":["1"],"url":"c.wasm"}
                ]
            }"#,
        )
        .expect("样例");
        let dups = reg.duplicate_site_ids();
        assert_eq!(dups.len(), 1, "只该报一组重复: {dups:?}");
        assert_eq!(dups[0].0, "x");
        assert_eq!(dups[0].1, vec!["a", "b"]);
    }

    #[test]
    fn distinct_site_ids_report_no_duplicates() {
        assert!(sample_registry().duplicate_site_ids().is_empty());
    }

    #[test]
    fn registry_entry_is_found_by_name() {
        let reg = sample_registry();
        assert_eq!(reg.find("fanqie").expect("存在").latest, "1.2.0");
    }

    #[test]
    fn registry_without_optional_digest_still_parses() {
        // 摘要字段是可选的：registry 早期版本没这两个字段，
        // 少了它们应该仍能装，而不是整个 registry 解析失败。
        let reg = sample_registry();
        assert!(reg.plugins[0].size.is_none());
        assert!(reg.plugins[0].sha256.is_none());
    }

    #[test]
    fn default_registry_is_the_published_site() {
        // 这个常量是用户 `ilp plugin install fanqie` 时实际会打到的地址。
        // 改域名时如果只改文档不改这里，用户装到的是旧站点；反之亦然。
        // 断言成字面量，就是要让任何一处改动都必须同时改这里。
        assert_eq!(DEFAULT_REGISTRY, "https://ilp.elysola.org");
        // 不带末尾斜杠：resolve_url 会自己拼 `/`，带两个会变成 `//`。
        assert!(!DEFAULT_REGISTRY.ends_with('/'));
        assert!(DEFAULT_REGISTRY.starts_with("https://"));
    }

    #[test]
    fn relative_urls_are_joined_against_the_mirror() {
        // registry 存相对路径，换域名不用重发；但绝对地址也要能用。
        let base = "https://mirror.example";
        assert_eq!(
            resolve_url(base, "fanqie/1.0.0/fanqie.wasm").expect("相对"),
            "https://mirror.example/fanqie/1.0.0/fanqie.wasm"
        );
        assert_eq!(
            resolve_url(base, "https://cdn.example/f.wasm").expect("绝对"),
            "https://cdn.example/f.wasm"
        );
        // 穿越与协议相对路径必须拒绝：否则 registry 能把下载引到任意位置。
        for bad in ["/etc/passwd", "../x.wasm", "a/../../x"] {
            assert!(resolve_url(base, bad).is_err(), "应拒绝: {bad}");
        }
    }

    #[test]
    fn artifact_size_mismatch_is_rejected() {
        // 截断的下载：头部可能仍是合法 wasm，只有长度对不上。
        let mut bytes = b"\0asm\x0d\0\x01\0".to_vec();
        bytes.extend_from_slice(&[0u8; 16]);
        let p = RegistryPlugin {
            name: "a".into(),
            site_id: "a".into(),
            abi: "0.1.0".into(),
            latest: "1".into(),
            versions: vec!["1".into()],
            url: "a.wasm".into(),
            size: Some(999),
            sha256: None,
            artifacts: HashMap::new(),
            signatures: HashMap::new(),
        };
        let err = format!("{:#}", verify_artifact(&bytes, &p).expect_err("应拒绝"));
        assert!(err.contains("大小不符"), "实得: {err}");
    }

    #[test]
    fn artifact_digest_mismatch_is_rejected() {
        let bytes = b"\0asm\x0d\0\x01\0".to_vec();
        let p = RegistryPlugin {
            name: "a".into(),
            site_id: "a".into(),
            abi: "0.1.0".into(),
            latest: "1".into(),
            versions: vec!["1".into()],
            url: "a.wasm".into(),
            size: Some(bytes.len() as u64),
            sha256: Some("0".repeat(64)),
            artifacts: HashMap::new(),
            signatures: HashMap::new(),
        };
        let err = format!("{:#}", verify_artifact(&bytes, &p).expect_err("应拒绝"));
        assert!(err.contains("校验和不符"), "实得: {err}");
    }

    #[test]
    fn revoked_signing_key_is_rejected() {
        use ed25519_dalek::SigningKey;
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let key_id = "revoked";
        let bytes = b"plugin";
        let signature = BASE64.encode(ed25519_dalek::Signer::sign(&signing, bytes).to_bytes());
        let plugin = RegistryPlugin {
            name: "a".into(),
            site_id: "a".into(),
            abi: "0.2.0".into(),
            latest: "1".into(),
            versions: vec!["1".into()],
            url: "a.wasm".into(),
            size: None,
            sha256: None,
            artifacts: HashMap::new(),
            signatures: [(
                "1".into(),
                RegistrySignatures::One(SignatureEntry {
                    key_id: key_id.into(),
                    signature,
                }),
            )]
            .into_iter()
            .collect(),
        };
        let mut config = Config::default();
        config.plugin_trusted_keys = [(
            key_id.into(),
            BASE64.encode(signing.verifying_key().to_bytes()),
        )]
        .into_iter()
        .collect();
        config.plugin_revoked_keys = vec![key_id.into()];
        config.require_plugin_signatures = true;
        let err = verify_signature(&config, &plugin, "1", bytes).expect_err("撤销密钥必须拒绝");
        assert!(format!("{err:#}").contains("已撤销"));
    }

    fn plugin_with_signatures(
        signatures: Vec<(&str, &ed25519_dalek::SigningKey)>,
        bytes: &[u8],
    ) -> RegistryPlugin {
        let sigs = signatures
            .into_iter()
            .map(|(key_id, signing)| SignatureEntry {
                key_id: key_id.into(),
                signature: BASE64.encode(ed25519_dalek::Signer::sign(signing, bytes).to_bytes()),
            })
            .collect::<Vec<_>>();
        RegistryPlugin {
            name: "a".into(),
            site_id: "a".into(),
            abi: "0.2.0".into(),
            latest: "1".into(),
            versions: vec!["1".into()],
            url: "a.wasm".into(),
            size: None,
            sha256: None,
            artifacts: HashMap::new(),
            signatures: [("1".to_string(), RegistrySignatures::Many(sigs))]
                .into_iter()
                .collect(),
        }
    }

    fn trust(keys: &[(&str, &ed25519_dalek::SigningKey)]) -> Config {
        let mut config = Config::default();
        config.plugin_trusted_keys = keys
            .iter()
            .map(|(id, signing)| {
                (
                    id.to_string(),
                    BASE64.encode(signing.verifying_key().to_bytes()),
                )
            })
            .collect();
        config.require_plugin_signatures = true;
        config
    }

    #[test]
    fn multi_signature_threshold_is_enforced() {
        use ed25519_dalek::SigningKey;
        let a = SigningKey::from_bytes(&[1u8; 32]);
        let b = SigningKey::from_bytes(&[2u8; 32]);
        let bytes = b"plugin";
        let plugin = plugin_with_signatures(vec![("a", &a), ("b", &b)], bytes);

        let mut config = trust(&[("a", &a), ("b", &b)]);
        config.plugin_signature_threshold = 2;
        verify_signature(&config, &plugin, "1", bytes).expect("双签应满足阈值 2");

        // 阈值提到 3，只有两个不同密钥，必须拒绝。
        config.plugin_signature_threshold = 3;
        let err = verify_signature(&config, &plugin, "1", bytes).expect_err("阈值不足必须拒绝");
        assert!(
            format!("{err:#}").contains("未达到要求的 3"),
            "实得: {err:#}"
        );
    }

    #[test]
    fn duplicated_key_does_not_count_twice() {
        use ed25519_dalek::SigningKey;
        let a = SigningKey::from_bytes(&[3u8; 32]);
        let bytes = b"plugin";
        // 同一把密钥签两次：不能算两个签名者。
        let plugin = plugin_with_signatures(vec![("a", &a), ("a", &a)], bytes);
        let mut config = trust(&[("a", &a)]);
        config.plugin_signature_threshold = 2;
        let err = verify_signature(&config, &plugin, "1", bytes).expect_err("重复密钥不能凑够阈值");
        assert!(
            format!("{err:#}").contains("未达到要求的 2"),
            "实得: {err:#}"
        );
    }

    #[test]
    fn unknown_signer_is_ignored_when_threshold_met() {
        use ed25519_dalek::SigningKey;
        let a = SigningKey::from_bytes(&[4u8; 32]);
        let b = SigningKey::from_bytes(&[5u8; 32]);
        let bytes = b"plugin";
        // b 未配置为信任根：它的签名必须被跳过，不能因为“有一行不认识”就整包失败。
        let plugin = plugin_with_signatures(vec![("a", &a), ("b", &b)], bytes);
        let config = trust(&[("a", &a)]);
        verify_signature(&config, &plugin, "1", bytes).expect("已信任的一签已足够");
    }

    #[test]
    fn legacy_single_signature_shape_still_parses() {
        let raw = r#"{
            "name":"a","site_id":"a","abi":"0.2.0","latest":"1","versions":["1"],
            "url":"a.wasm",
            "signatures":{"1":{"key_id":"k","signature":"c2ln"}}
        }"#;
        let plugin: RegistryPlugin = serde_json::from_str(raw).expect("旧单签格式仍可解析");
        let sigs = plugin.signatures.get("1").expect("有签名");
        assert_eq!(sigs.iter().count(), 1);
    }

    #[test]
    fn signature_array_shape_parses() {
        let raw = r#"{
            "name":"a","site_id":"a","abi":"0.2.0","latest":"1","versions":["1"],
            "url":"a.wasm",
            "signatures":{"1":[{"key_id":"k1","signature":"c2ln"},{"key_id":"k2","signature":"c2ln"}]}
        }"#;
        let plugin: RegistryPlugin = serde_json::from_str(raw).expect("多签格式可解析");
        let sigs = plugin.signatures.get("1").expect("有签名");
        assert_eq!(sigs.iter().count(), 2);
    }

    #[test]
    fn artifact_with_matching_digest_is_accepted() {
        use sha2::{Digest, Sha256};
        let bytes = b"\0asm\x0d\0\x01\0".to_vec();
        let p = RegistryPlugin {
            name: "a".into(),
            site_id: "a".into(),
            abi: "0.1.0".into(),
            latest: "1".into(),
            versions: vec!["1".into()],
            url: "a.wasm".into(),
            size: Some(bytes.len() as u64),
            sha256: Some(format!("{:x}", Sha256::digest(&bytes))),
            artifacts: HashMap::new(),
            signatures: HashMap::new(),
        };
        assert!(verify_artifact(&bytes, &p).is_ok());
    }
}
