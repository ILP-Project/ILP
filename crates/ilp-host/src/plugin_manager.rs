use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use sha2::Digest;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::cli::PluginCommand;
use crate::config::Config;
use crate::signing::SignatureSet;
use ilp_types::PluginMetadata;

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

/// 宿主自身版本。插件用 `min_host_version`/`max_host_version` 声明兼容区间。
pub const HOST_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 把 `x.y.z` 解析成可比较的三元组；多段或非数字返回 None。
///
/// 只接受三段：补零对齐的区间比较在段数不一致时会算错
/// （`1.2` 到底相当于 `1.2.0` 还是 `1.2.*`？两种都有实现，不如不支持）。
fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.split('-').next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// 宿主版本是否落在插件声明的兼容区间内。
///
/// 缺字段视为“未声明”，一律放行：旧 registry 里没有这些字段，
/// 把它们当成“不兼容任何宿主”会让所有已发布插件都装不上。
/// 预发布后缀在比较时被忽略，理由与发布侧 build_site.py 一致。
fn host_version_compatible(min: Option<&str>, max: Option<&str>) -> Result<()> {
    let Some(this) = parse_version(HOST_VERSION) else {
        // 宿主版本号本身有问题，属于打包错误，不该归咎于插件。
        return Ok(());
    };
    if let Some(min) = min {
        let bound = parse_version(min)
            .ok_or_else(|| anyhow!("插件声明 min_host_version={min} 不是合法的 x.y.z"))?;
        if this < bound {
            return Err(anyhow!(
                "插件要求 ilp >= {min}，当前是 {HOST_VERSION}。请升级 ilp"
            ));
        }
    }
    if let Some(max) = max {
        let bound = parse_version(max)
            .ok_or_else(|| anyhow!("插件声明 max_host_version={max} 不是合法的 x.y.z"))?;
        if this > bound {
            return Err(anyhow!(
                "插件只支持到 ilp {max}，当前是 {HOST_VERSION}。\n\
                 ABI 兼容不代表行为兼容——请升级插件，或安装更旧的 ilp"
            ));
        }
    }
    Ok(())
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
    /// 插件声明的宿主兼容范围（闭区间）。旧 registry 没有这两个字段。
    #[serde(default)]
    min_host_version: Option<String>,
    #[serde(default)]
    max_host_version: Option<String>,
    /// registry 声称插件申请哪些权限。
    ///
    /// 这只是**声明**，安装时会与 WASM 自己的 metadata 逐项比对：
    /// registry 是展示面，插件作者完全可以让它说一套、代码里做另一套。
    /// 不改的话，`plugin info` 展示的权限与实际情况无关。
    #[serde(default)]
    permissions: Vec<String>,
    #[serde(default)]
    allowed_domains: Vec<String>,
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
        PluginCommand::New {
            name,
            site_name,
            site_id,
            domain,
            dir,
        } => new_plugin(&name, site_name, site_id, domain, dir),
        PluginCommand::List => list(config),
        PluginCommand::Info { name } => info(config, &name),
        PluginCommand::Install { source } => install(config, &source),
        PluginCommand::Update { name, apply } => update(config, &name, apply),
        PluginCommand::Uninstall { name } => uninstall(config, &name),
        PluginCommand::Enable { name } => enable(config, &name),
        PluginCommand::Disable { name } => disable(config, &name),
    }
}

/// 模板里的占位符。故意用 `@名字@` 而不是 `{{name}}`：
/// 模板里本来就有 Rust 的 `{}` 格式串和 TOML，用大括号会撞。
const TEMPLATE_FILES: &[(&str, &str)] = &[
    (
        "Cargo.toml",
        include_str!("../../../plugins/template/Cargo.toml"),
    ),
    (
        "plugin.toml",
        include_str!("../../../plugins/template/plugin.toml"),
    ),
    (
        "src/lib.rs",
        include_str!("../../../plugins/template/src/lib.rs"),
    ),
    (
        "src/fixture.rs",
        include_str!("../../../plugins/template/src/fixture.rs"),
    ),
    (
        "fixtures/README.md",
        include_str!("../../../plugins/template/fixtures/README.md"),
    ),
    (
        "fixtures/example-http.json",
        include_str!("../../../plugins/template/fixtures/example-http.json"),
    ),
    (
        "README.md",
        include_str!("../../../plugins/template/README.md"),
    ),
];

/// 生成插件骨架。
///
/// 目标是让「第三条命令就能跑起来」成立：`new` → `cargo build --target
/// wasm32-wasip2` → `ilp plugin install ./<dir>/target/.../<name>.wasm`。
///
/// 缺省值故意从 `name` 推导而不是要求全填：刚上手的人还不知道 site_id
/// 和域名该怎么写，逼他先做决定只会让他去抄别人的。
fn new_plugin(
    name: &str,
    site_name: Option<String>,
    site_id: Option<String>,
    domain: Option<String>,
    dir: Option<PathBuf>,
) -> Result<()> {
    validate_plugin_name(name)?;
    let parent = dir.unwrap_or_else(|| PathBuf::from("."));
    let target = parent.join(name);
    if target.exists() {
        return Err(anyhow!(
            "目录 {} 已存在。换个名字，或先删掉它",
            target.display()
        ));
    }

    // Cargo 包名不能直接用插件名里的下划线/点，统一成连字符再校验。
    let crate_name = name.replace(['_', '.'], "-");
    let site_id = site_id.unwrap_or_else(|| crate_name.clone());
    let site_name = site_name.unwrap_or_else(|| format!("{name} 站点"));
    let domain = domain.unwrap_or_else(|| format!("{site_id}.example.com"));
    validate_site_id(&site_id)?;
    validate_domain(&domain)?;

    let replacements = [
        ("@PLUGIN_NAME@", name),
        ("@CRATE_NAME@", &crate_name),
        ("@SITE_ID@", &site_id),
        ("@SITE_NAME@", &site_name),
        ("@DOMAIN@", &domain),
    ];

    for (rel, content) in TEMPLATE_FILES {
        let mut body = (*content).to_string();
        for (needle, value) in &replacements {
            body = body.replace(needle, value);
        }
        let path = target.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建目录 {} 失败", parent.display()))?;
        }
        std::fs::write(&path, body).with_context(|| format!("写入 {} 失败", path.display()))?;
    }

    println!("已生成插件骨架: {}", target.display());
    println!();
    println!("下一步:");
    println!(
        "  1. 编辑 {}/plugin.toml：填写 data_source 与 authorization_boundary",
        target.display()
    );
    println!("  2. 实现 {}/src/lib.rs 里的抓取逻辑", target.display());
    println!(
        "  3. cargo build --target wasm32-wasip2 --release --manifest-path {}/Cargo.toml",
        target.display()
    );
    // 产物名是 Cargo 把包名的 `-` 换成 `_` 再加 `ilp_plugin_` 前缀，不是包名本身。
    // 提示里写错会让用户对着一个不存在的路径反复重试。
    let artifact = format!("ilp_plugin_{}.wasm", crate_name.replace('-', "_"));
    println!(
        "  4. ilp plugin install {}/target/wasm32-wasip2/release/{artifact}",
        target.display()
    );
    Ok(())
}

/// 插件名会变成目录名和 Cargo 包名，必须限定在安全字符集内。
fn validate_plugin_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 {
        return Err(anyhow!("插件名长度必须在 1–64 之间"));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(anyhow!(
            "插件名 {name} 含非法字符（只允许字母、数字、- 和 _）"
        ));
    }
    if name.starts_with('-') || name.starts_with('_') {
        return Err(anyhow!("插件名不能以 - 或 _ 开头"));
    }
    Ok(())
}

/// site_id 会出现在 URL 查询和文件名里，限制比插件名更严：不允许下划线。
///
/// 与 `install` 侧的校验保持一致——这里放过、那边拒绝，用户会在
/// 构建完之后才撞墙。
fn validate_site_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 64 {
        return Err(anyhow!("站点 ID 长度必须在 1–64 之间"));
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(anyhow!(
            "站点 ID {id} 含非法字符（只允许字母、数字、- 和 _）"
        ));
    }
    Ok(())
}

/// 域名必须能被 `allowed_domains` 的匹配规则接受：不带协议、不带路径、不带端口，
/// 也不允许通配符（宿主按标签边界精确匹配，通配符从来就不生效）。
fn validate_domain(domain: &str) -> Result<()> {
    if domain.is_empty() {
        return Err(anyhow!("域名不能为空"));
    }
    if domain.contains("://") || domain.contains('/') || domain.contains(':') {
        return Err(anyhow!(
            "域名 {domain} 只能写主机名，不能带协议、路径或端口"
        ));
    }
    if domain.contains('*') {
        return Err(anyhow!("域名 {domain} 不支持通配符，请写明具体域名"));
    }
    if !domain.contains('.') {
        return Err(anyhow!("域名 {domain} 至少要有一个点"));
    }
    Ok(())
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
            permissions: Vec::new(),
            allowed_domains: Vec::new(),
            min_host_version: None,
            max_host_version: None,
        },
    )?;
    let digest = format!("{:x}", sha2::Sha256::digest(&bytes));
    println!(
        "插件: {name}\n文件: {}\n大小: {} 字节\nSHA-256: {digest}",
        path.display(),
        bytes.len()
    );

    // 权限与域名只能从 WASM 自己的 metadata 读：registry 里那份是展示用的，
    // 不能当作事实。用户要看“这个插件到底能干什么”，必须看真实声明。
    let storage_root = Path::new(&config.data_folder).join("plugins");
    match crate::engine::PluginInstance::load(&path, &storage_root) {
        Ok(plugin) => print_metadata(&plugin.metadata),
        // 加载失败不该让 info 整个失败：元数据与完整性信息本身是有用的。
        Err(e) => println!("\n无法读取插件内声明的元数据: {e:#}"),
    }
    Ok(())
}

fn print_metadata(m: &PluginMetadata) {
    println!("\n站点: {}（{}）", m.site_name, m.site_domain);
    println!("站点 ID: {}", m.site_id);
    println!("插件: {} {}", m.plugin_name, m.version);
    println!("ABI: {}", m.abi_version);
    if m.permissions.is_empty() {
        println!("申请的权限: 无");
    } else {
        let mut perms = m.permissions.clone();
        perms.sort();
        println!("申请的权限: {}", perms.join(", "));
    }
    if m.allowed_domains.is_empty() {
        println!("声明的域名: 未限制");
    } else {
        let mut domains = m.allowed_domains.clone();
        domains.sort();
        println!("声明的域名: {}", domains.join(", "));
    }
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
        permissions: p.permissions.clone(),
        allowed_domains: p.allowed_domains.clone(),
        min_host_version: p.min_host_version.clone(),
        max_host_version: p.max_host_version.clone(),
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

fn update(config: &Config, name: &str, apply: bool) -> Result<()> {
    let name = safe_plugin_name(name)?;
    let installed = config.plugins_dir().join(format!("{name}.wasm"));
    if !installed.is_file() {
        return Err(anyhow!(
            "插件 {name} 尚未安装，请先运行 ilp plugin install {name}"
        ));
    }
    let base = registry_base();
    let raw = crate::http::get_bytes(&format!("{base}/registry.json"))
        .context("获取插件 registry 失败（可用 ILP_REGISTRY_URL 指定镜像）")?;
    verify_registry_signature(config, &base, &raw)?;
    let reg: Registry = serde_json::from_slice(&raw).context("解析 registry.json 失败")?;
    let plugin = reg.find(name)?;
    let current = crate::engine::PluginInstance::load(
        &installed,
        &Path::new(&config.data_folder).join("plugins"),
    )
    .with_context(|| format!("读取已安装插件 {name} 失败"))?;
    let latest = &plugin.latest;
    println!(
        "插件 {name}: 当前 {}，registry latest {}",
        current.metadata.version, latest
    );
    if current.metadata.version == *latest {
        println!("插件 {name} 已是最新版本");
        return Ok(());
    }
    println!("可用更新: {name} {} -> {latest}", current.metadata.version);
    if !apply {
        println!("预览模式：使用 `ilp plugin update {name} --apply` 应用更新");
        return Ok(());
    }
    install(config, &format!("{name}@{latest}"))
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
    // 宿主版本范围与 ABI 是两件事：ABI 锁接口形状，范围锁行为。
    host_version_compatible(p.min_host_version.as_deref(), p.max_host_version.as_deref())
        .with_context(|| format!("插件 {name} 与当前 ilp 版本不兼容"))?;

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
    // 先落到暂存目录再改名：下面的能力核对需要真的加载组件，
    // 而加载失败的半成品不该已经占到最终位置。
    let staging_dir = dir.join("_staging");
    fs::create_dir_all(&staging_dir)?;
    let staging = staging_dir.join(format!("{name}.wasm"));
    write_atomic(&staging, &bytes)?;
    let verdict = check_declared_capabilities(config, p, &staging);
    let _ = fs::remove_dir_all(&staging_dir);
    verdict?;

    write_atomic(&dest, &bytes)?;
    println!("已安装插件 {name} {want} 到 {}", dest.display());
    Ok(())
}

/// 核对 registry 声明的权限与 WASM 自己的声明。
///
/// 宿主的安全策略取的是 **WASM metadata**，registry 那份只用于展示与审核。
/// 两份不一致时，用户看到的权限就不是他实际拿到的权限。
///
/// 规则不对称：
///
/// - WASM 比注册表**多**申请一项能力 → 拒绝安装。这是信息隐瞒，
///   必须保证“安装前看到的”和“装完跑起来的”一致；
/// - WASM 比注册表**少** → 仅告警。注册表写多了是文档过错，不影响安全。
///
/// 注册表什么都没写时跳过：旧镜像没这些字段，不能因此拒绝所有旧插件。
fn check_declared_capabilities(config: &Config, p: &RegistryPlugin, wasm: &Path) -> Result<()> {
    if p.permissions.is_empty() && p.allowed_domains.is_empty() {
        return Ok(());
    }
    let storage_root = Path::new(&config.data_folder).join("plugins");
    let plugin = crate::engine::PluginInstance::load(wasm, &storage_root)
        .with_context(|| format!("插件 {} 的元数据校验未通过（组件无法加载）", p.name))?;
    for warning in compare_capabilities(&p.permissions, &p.allowed_domains, &plugin.metadata)? {
        tracing::warn!("插件 {}: {warning}", p.name);
    }
    Ok(())
}

/// `check_declared_capabilities` 的纯逻辑部分：不碰磁盘，可以直接测。
///
/// 返回的字符串是“注册表多写了”的告警；真正的不一致直接返回错误。
fn compare_capabilities(
    declared_permissions: &[String],
    declared_domains: &[String],
    actual: &PluginMetadata,
) -> Result<Vec<String>> {
    let mut warnings = Vec::new();
    let declared: std::collections::HashSet<&str> =
        declared_permissions.iter().map(String::as_str).collect();
    let mut undeclared: Vec<&str> = actual
        .permissions
        .iter()
        .map(String::as_str)
        .filter(|perm| !declared.contains(perm))
        .collect();
    if !undeclared.is_empty() {
        undeclared.sort_unstable();
        let mut declared_sorted: Vec<&str> = declared.into_iter().collect();
        declared_sorted.sort_unstable();
        return Err(anyhow!(
            "插件在 registry 里申报的权限是 [{}]，但 WASM 自己申请了额外权限 [{}]。\n\
             注册表隐瞒了插件真实能做的事，已拒绝安装",
            declared_sorted.join(", "),
            undeclared.join(", ")
        ));
    }
    let mut unused: Vec<&str> = declared_permissions
        .iter()
        .map(String::as_str)
        .filter(|perm| !actual.permissions.iter().any(|a| a == perm))
        .collect();
    if !unused.is_empty() {
        unused.sort_unstable();
        warnings.push(format!(
            "registry 里多申报了权限 [{}]，WASM 实际未申请。以 WASM 为准",
            unused.join(", ")
        ));
    }

    // 域名同理：插件能连到注册表没写的主机，用户装的就不是他以为的东西。
    // 空列表的含义是“不作限制”，所以只在注册表真的写了限制时才比。
    if !declared_domains.is_empty() {
        let allowed: std::collections::HashSet<&str> =
            declared_domains.iter().map(String::as_str).collect();
        let mut extra: Vec<&str> = actual
            .allowed_domains
            .iter()
            .map(String::as_str)
            .filter(|d| !allowed.contains(d))
            .collect();
        if !extra.is_empty() {
            extra.sort_unstable();
            return Err(anyhow!(
                "插件在 registry 里申报的域名是 [{}]，但 WASM 自己声明了额外域名 [{}]。\n\
                 注册表隐瞒了插件真实访问范围，已拒绝安装",
                declared_domains.join(", "),
                extra.join(", ")
            ));
        }
    }
    Ok(warnings)
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
                "registry 元数据没有签名（{sig_url} 不存在），默认已启用强制校验。\
                 请使用带签名的镜像；仅内部镜像可在 config.json 中显式关闭 require_registry_signatures"
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
            permissions: Vec::new(),
            allowed_domains: Vec::new(),
            min_host_version: None,
            max_host_version: None,
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
            permissions: Vec::new(),
            allowed_domains: Vec::new(),
            min_host_version: None,
            max_host_version: None,
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
            permissions: Vec::new(),
            allowed_domains: Vec::new(),
            min_host_version: None,
            max_host_version: None,
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
            permissions: Vec::new(),
            allowed_domains: Vec::new(),
            min_host_version: None,
            max_host_version: None,
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
            permissions: Vec::new(),
            allowed_domains: Vec::new(),
            min_host_version: None,
            max_host_version: None,
        };
        assert!(verify_artifact(&bytes, &p).is_ok());
    }

    #[test]
    fn template_files_are_embedded_and_placeholder_driven() {
        // 生成器把模板编译进二进制。模板被挪走或改名时，
        // include_str! 会先报错——这条测试守的是「占位符没被写死回字面量」。
        for (rel, content) in TEMPLATE_FILES {
            assert!(!content.is_empty(), "{rel} 是空的");
        }
        let plugin_toml = TEMPLATE_FILES
            .iter()
            .find(|(rel, _)| *rel == "plugin.toml")
            .map(|(_, c)| *c)
            .expect("模板必须有 plugin.toml");
        for needle in ["@PLUGIN_NAME@", "@SITE_ID@", "@SITE_NAME@", "@DOMAIN@"] {
            assert!(
                plugin_toml.contains(needle),
                "plugin.toml 缺少占位符 {needle}，生成出来的插件会带模板的假数据"
            );
        }
    }

    #[test]
    fn new_plugin_replaces_every_placeholder() {
        let dir = std::env::temp_dir().join(format!("ilp-new-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        new_plugin(
            "demo",
            Some("示例站点".into()),
            Some("demo-id".into()),
            Some("demo.com".into()),
            Some(dir.clone()),
        )
        .expect("生成插件骨架");

        let root = dir.join("demo");
        for (rel, _) in TEMPLATE_FILES {
            let path = root.join(rel);
            assert!(path.exists(), "{rel} 没写出来");
            let body = std::fs::read_to_string(&path).unwrap();
            // 漏替换的占位符会让生成的插件带着模板数据跑起来，很难发现。
            assert!(
                !body.contains('@') || !body.contains("@PLUGIN_NAME@"),
                "{rel} 残留占位符"
            );
            assert!(!body.contains("@SITE_ID@"), "{rel} 残留 @SITE_ID@");
            assert!(!body.contains("@SITE_NAME@"), "{rel} 残留 @SITE_NAME@");
            assert!(!body.contains("@DOMAIN@"), "{rel} 残留 @DOMAIN@");
            assert!(!body.contains("@CRATE_NAME@"), "{rel} 残留 @CRATE_NAME@");
        }

        let manifest = std::fs::read_to_string(root.join("plugin.toml")).unwrap();
        assert!(manifest.contains("name = \"demo\""));
        assert!(manifest.contains("site_id = \"demo-id\""));
        assert!(manifest.contains("allowed_domains = [\"demo.com\"]"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_plugin_refuses_to_overwrite_existing_directory() {
        let dir = std::env::temp_dir().join(format!("ilp-newdup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("demo")).unwrap();
        let err = new_plugin("demo", None, None, None, Some(dir.clone())).expect_err("应拒绝");
        assert!(format!("{err}").contains("已存在"), "实得: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_plugin_rejects_unsafe_names_and_domains() {
        let dir = std::env::temp_dir().join(format!("ilp-newbad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 名字会变成目录名，`../` 必须挡在写文件之前。
        assert!(new_plugin("../evil", None, None, None, Some(dir.clone())).is_err());
        assert!(new_plugin("a/b", None, None, None, Some(dir.clone())).is_err());
        // 域名写法错了，宿主那边永远匹配不上，早点拒绝。
        assert!(new_plugin(
            "d1",
            None,
            None,
            Some("https://x.com".into()),
            Some(dir.clone())
        )
        .is_err());
        assert!(new_plugin("d2", None, None, Some("*.x.com".into()), Some(dir.clone())).is_err());
        assert!(new_plugin(
            "d3",
            None,
            None,
            Some("x.com:8080".into()),
            Some(dir.clone())
        )
        .is_err());
        assert!(new_plugin("d4", None, None, Some("nodot".into()), Some(dir.clone())).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn version_parsing_rejects_ambiguous_shapes() {
        assert_eq!(parse_version("0.1.0"), Some((0, 1, 0)));
        assert_eq!(parse_version("1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.2.3-rc1"), Some((1, 2, 3)));
        // 两段和四段都不接受：补零对齐的区间比较在段数不一致时会有歧义。
        assert_eq!(parse_version("1.2"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
        assert_eq!(parse_version("1.2.x"), None);
        assert_eq!(parse_version(""), None);
        // 字典序会把 1.10.0 排在 1.9.0 前面，数值比较才正确。
        assert!(parse_version("1.10.0") > parse_version("1.9.0"));
    }

    #[test]
    fn host_version_outside_declared_range_is_rejected() {
        // 上限低于当前宿主：插件只支持到旧版本，装上去就会静默失效。
        let err = format!(
            "{:#}",
            host_version_compatible(Some("0.1.0"), Some("0.0.99")).expect_err("应拒绝")
        );
        assert!(err.contains("只支持到"), "实得: {err}");

        // 下限高于当前宿主：插件用到了更新的宿主行为。
        let err = format!(
            "{:#}",
            host_version_compatible(Some("99.0.0"), None).expect_err("应拒绝")
        );
        assert!(err.contains("要求 ilp >="), "实得: {err}");
    }

    #[test]
    fn missing_version_bounds_are_permissive() {
        // 旧 registry 没有这两个字段。若当成“不兼容任何宿主”，
        // 所有已发布插件都会装不上。
        assert!(host_version_compatible(None, None).is_ok());
        assert!(host_version_compatible(Some("0.1.0"), None).is_ok());
        // 当前宿主是 0.1.0，落在任意宽松上限内。
        assert!(host_version_compatible(None, Some("99.0.0")).is_ok());
    }

    #[test]
    fn malformed_bounds_are_reported_not_ignored() {
        // 写错版本号时静默放行，等于这道门禁不存在。
        assert!(host_version_compatible(Some("1.2"), None).is_err());
        assert!(host_version_compatible(None, Some("latest")).is_err());
    }

    fn meta(permissions: &[&str], domains: &[&str]) -> PluginMetadata {
        PluginMetadata {
            abi_version: "0.2.0".into(),
            site_name: "s".into(),
            site_id: "s".into(),
            site_domain: "example.com".into(),
            plugin_name: "p".into(),
            plugin_id: "p".into(),
            version: "1".into(),
            permissions: permissions.iter().map(|s| s.to_string()).collect(),
            allowed_domains: domains.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn owned(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn registry_hiding_a_permission_is_rejected() {
        // registry 说只要 http，WASM 里却还要 secret-read：
        // 用户看到的权限清单是假的，必须拒绝安装。
        let declared = owned(&["http"]);
        let actual = meta(&["http", "secret-read"], &[]);
        let err = format!(
            "{:#}",
            compare_capabilities(&declared, &[], &actual).expect_err("应拒绝")
        );
        assert!(err.contains("额外权限"), "实得: {err}");
        assert!(err.contains("secret-read"), "实得: {err}");
    }

    #[test]
    fn registry_declaring_more_than_wasm_is_only_a_warning() {
        // 反方向只是文档写错了，不构成安全风险，不该拦下载。
        let declared = owned(&["http", "clock", "storage-read"]);
        let actual = meta(&["http"], &[]);
        let warnings = compare_capabilities(&declared, &[], &actual).expect("不该失败");
        assert_eq!(warnings.len(), 1, "实得: {warnings:?}");
        assert!(warnings[0].contains("clock"), "实得: {warnings:?}");
        assert!(warnings[0].contains("storage-read"), "实得: {warnings:?}");
    }

    #[test]
    fn matching_declarations_pass_silently() {
        let declared = owned(&["http", "clock"]);
        let actual = meta(&["clock", "http"], &["a.com"]);
        // 顺序不同不算差异：HashSet 比较。
        assert!(compare_capabilities(&declared, &owned(&["a.com"]), &actual)
            .expect("不该失败")
            .is_empty());
    }

    #[test]
    fn registry_hiding_a_domain_is_rejected() {
        let declared_perms = owned(&["http"]);
        let actual = meta(&["http"], &["fanqienovel.com", "evil.example"]);
        let err = format!(
            "{:#}",
            compare_capabilities(&declared_perms, &owned(&["fanqienovel.com"]), &actual)
                .expect_err("应拒绝")
        );
        assert!(err.contains("额外域名"), "实得: {err}");
        assert!(err.contains("evil.example"), "实得: {err}");
    }

    #[test]
    fn empty_registry_domains_mean_unrestricted() {
        // 旧镜像没有 allowed_domains 字段。若把它当成"插件不许连任何域名"，
        // 所有旧版本都会装不上。
        let declared = owned(&["http"]);
        let actual = meta(&["http"], &["fanqienovel.com"]);
        assert!(compare_capabilities(&declared, &[], &actual)
            .expect("不该失败")
            .is_empty());
    }
}
