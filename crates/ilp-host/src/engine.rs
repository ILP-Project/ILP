use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use wasmtime::component::{Component, Linker, Resource};
use wasmtime::{Config as WasmtimeConfig, Engine, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{ResourceTable, WasiCtx, WasiCtxView, WasiView};

pub struct HttpCapability;
pub struct StorageCapability;
pub struct SecretCapability;

const MAX_PLUGIN_MEMORY: usize = 64 * 1024 * 1024;
const MAX_PLUGIN_FUEL: u64 = 50_000_000;
const MAX_HTTP_BODY: usize = 2 * 1024 * 1024;
const MAX_HTTP_RESPONSE: usize = 16 * 1024 * 1024;
const MAX_HTTP_HEADERS: usize = 128;
const MAX_HTTP_HEADER_VALUE: usize = 16 * 1024;
const MAX_FETCH_TASKS: usize = 128;
const MAX_STORAGE_VALUE: usize = 4 * 1024 * 1024;
const MAX_LOG_MESSAGE: usize = 16 * 1024;

#[derive(Clone, Debug)]
struct CapabilityPolicy {
    permissions: std::collections::HashSet<String>,
    allowed_domains: Vec<String>,
}

impl CapabilityPolicy {
    fn unrestricted() -> Self {
        Self {
            permissions: [
                "http",
                "clock",
                "storage-read",
                "storage-write",
                "secret-read",
                "secret-write",
            ]
            .into_iter()
            .map(str::to_string)
            .collect(),
            allowed_domains: Vec::new(),
        }
    }

    fn allows(&self, permission: &str) -> bool {
        self.permissions.contains(permission)
    }

    fn allows_url(&self, url: &str) -> Result<(), String> {
        let parsed = reqwest::Url::parse(url).map_err(|e| format!("invalid-url: {e}"))?;
        if parsed.scheme() != "https" {
            return Err("permission-denied: only https is allowed".to_string());
        }
        let host = parsed
            .host_str()
            .ok_or_else(|| "invalid-url: missing host".to_string())?;
        if self.allowed_domains.is_empty()
            || self
                .allowed_domains
                .iter()
                .any(|domain| domain_matches(host, domain))
        {
            Ok(())
        } else {
            Err(format!("permission-denied: domain {host}"))
        }
    }
}

fn domain_matches(host: &str, domain: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let domain = domain
        .trim()
        .trim_start_matches("*.")
        .trim_end_matches('.')
        .to_ascii_lowercase();
    host == domain || host.ends_with(&format!(".{domain}"))
}

fn permission_name(permission: current::ilp::plugin::types::Permission) -> String {
    match permission {
        current::ilp::plugin::types::Permission::Http => "http",
        current::ilp::plugin::types::Permission::Clock => "clock",
        current::ilp::plugin::types::Permission::StorageRead => "storage-read",
        current::ilp::plugin::types::Permission::StorageWrite => "storage-write",
        current::ilp::plugin::types::Permission::SecretRead => "secret-read",
        current::ilp::plugin::types::Permission::SecretWrite => "secret-write",
    }
    .to_string()
}

mod current {
    wasmtime::component::bindgen!({
        world: "novel-plugin",
        path: "../ilp-plugin-wit/wit/v2",
        with: {
            "ilp:plugin/host.http-handle": super::HttpCapability,
            "ilp:plugin/host.storage-handle": super::StorageCapability,
            "ilp:plugin/host.secret-handle": super::SecretCapability,
        },
    });
}

mod legacy {
    wasmtime::component::bindgen!({
        world: "novel-plugin",
        path: "../ilp-plugin-wit/wit/v1",
    });
}

use current::ilp;
use current::ilp::plugin::host::Host as HostTrait;
use current::ilp::plugin::types::{FetchTask, HttpResponse};

pub use current::ilp::plugin::types as wit_types;

use ilp_types::{ChapterContent, ChapterEntry, PluginMetadata, SearchResult};

/// 宿主给插件的全部能力对应的状态。
pub struct HostState {
    ctx: WasiCtx,
    table: ResourceTable,
    /// 插件名，用于隔离 storage 命名空间
    plugin_name: String,
    storage_dir: PathBuf,
    secret_dir: PathBuf,
    /// 内存里的 KV，落盘前先命中这里
    cache: HashMap<String, Vec<u8>>,
    policy: CapabilityPolicy,
    limits: StoreLimits,
}

impl HostState {
    fn new(plugin_name: String, storage_dir: PathBuf) -> Self {
        let secret_dir = storage_dir
            .parent()
            .unwrap_or(Path::new("."))
            .join("secrets")
            .join(&plugin_name);
        Self::with_policy_and_secret(
            plugin_name,
            storage_dir,
            secret_dir,
            CapabilityPolicy::unrestricted(),
        )
    }

    #[cfg(test)]
    fn with_policy(plugin_name: String, storage_dir: PathBuf, policy: CapabilityPolicy) -> Self {
        let plugin_name_for_secret = plugin_name.clone();
        let secret_dir = storage_dir
            .parent()
            .unwrap_or(Path::new("."))
            .join("secrets")
            .join(plugin_name_for_secret);
        Self::with_policy_and_secret(plugin_name, storage_dir, secret_dir, policy)
    }

    fn with_policy_and_secret(
        plugin_name: String,
        storage_dir: PathBuf,
        secret_dir: PathBuf,
        policy: CapabilityPolicy,
    ) -> Self {
        Self {
            ctx: WasiCtx::builder().build(),
            table: ResourceTable::new(),
            plugin_name,
            storage_dir,
            secret_dir,
            cache: HashMap::new(),
            policy,
            limits: StoreLimitsBuilder::new()
                .memory_size(MAX_PLUGIN_MEMORY)
                .instances(8)
                .tables(16)
                .memories(4)
                .trap_on_grow_failure(true)
                .build(),
        }
    }
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

fn level_to_tracing(level: u8, plugin: &str, msg: &str) {
    match level {
        0 => tracing::error!(target: "plugin", "[{plugin}] {msg}"),
        1 => tracing::warn!(target: "plugin", "[{plugin}] {msg}"),
        2 => tracing::info!(target: "plugin", "[{plugin}] {msg}"),
        3 => tracing::debug!(target: "plugin", "[{plugin}] {msg}"),
        _ => tracing::trace!(target: "plugin", "[{plugin}] {msg}"),
    }
}

impl HostTrait for HostState {
    fn log(&mut self, level: u8, message: String) {
        if message.len() > MAX_LOG_MESSAGE {
            tracing::warn!("插件 {} 日志过长，已丢弃", self.plugin_name);
            return;
        }
        level_to_tracing(level, &self.plugin_name, &message);
    }

    /// 返回 UTC epoch 毫秒。签名与设备上报都要真实墙钟时间，
    /// 单调计时器（进程启动后的 elapsed）会让服务端判定客户端异常。
    fn now_ms(&mut self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    fn http_fetch(
        &mut self,
        request: ilp::plugin::types::HttpRequest,
    ) -> Result<ilp::plugin::types::HttpResponse, String> {
        validate_http_request(&request)?;
        self.policy.allows_url(&request.url)?;
        let response = crate::http::send(&request).map_err(|e| format!("{e:#}"))?;
        validate_http_response(&response)?;
        Ok(response)
    }

    fn acquire_http(&mut self) -> Option<Resource<HttpCapability>> {
        self.policy
            .allows("http")
            .then(|| self.table.push(HttpCapability).ok())
            .flatten()
    }

    fn acquire_storage(&mut self) -> Option<Resource<StorageCapability>> {
        (self.policy.allows("storage-read") || self.policy.allows("storage-write"))
            .then(|| self.table.push(StorageCapability).ok())
            .flatten()
    }

    fn acquire_secret(&mut self) -> Option<Resource<SecretCapability>> {
        (self.policy.allows("secret-read") || self.policy.allows("secret-write"))
            .then(|| self.table.push(SecretCapability).ok())
            .flatten()
    }

    fn storage_get(&mut self, key: String) -> Option<Vec<u8>> {
        if !self.policy.allows("storage-read") {
            return None;
        }
        if let Some(v) = self.cache.get(&key) {
            return Some(v.clone());
        }
        // key 只允许简单标识符，防止穿越到 storage 目录之外
        if !is_safe_key(&key) {
            tracing::warn!("插件 {} 请求了非法 storage key: {key}", self.plugin_name);
            return None;
        }
        let path = self.storage_dir.join(&key);
        let v = std::fs::read(path).ok()?;
        self.cache.insert(key, v.clone());
        Some(v)
    }

    fn storage_set(&mut self, key: String, value: Vec<u8>) -> Result<(), String> {
        if !self.policy.allows("storage-write") {
            return Err("permission-denied: storage-write".to_string());
        }
        if !is_safe_key(&key) {
            return Err(format!("非法 storage key: {key}"));
        }
        if value.len() > MAX_STORAGE_VALUE {
            return Err(format!("storage value 过大: {} 字节", value.len()));
        }
        std::fs::create_dir_all(&self.storage_dir).map_err(|e| e.to_string())?;
        atomic_write(&self.storage_dir.join(&key), &value).map_err(|e| e.to_string())?;
        self.cache.insert(key, value);
        Ok(())
    }

    fn storage_delete(&mut self, key: String) -> Result<(), String> {
        if !self.policy.allows("storage-write") {
            return Err("permission-denied: storage-write".to_string());
        }
        if !is_safe_key(&key) {
            return Err(format!("非法 storage key: {key}"));
        }
        self.cache.remove(&key);
        let path = self.storage_dir.join(&key);
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

impl ilp::plugin::types::Host for HostState {}

impl legacy::ilp::plugin::host::Host for HostState {
    fn log(&mut self, level: u8, message: String) {
        level_to_tracing(level, &self.plugin_name, &message);
    }

    fn now_ms(&mut self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    fn http_fetch(
        &mut self,
        request: legacy::ilp::plugin::host::HttpRequest,
    ) -> Result<legacy::ilp::plugin::host::HttpResponse, String> {
        crate::http::validate_plugin_url(&request.url).map_err(|e| format!("{e:#}"))?;
        validate_legacy_http_request(&request)?;
        self.policy.allows_url(&request.url)?;
        let request = ilp::plugin::types::HttpRequest {
            method: request.method,
            url: request.url,
            headers: request
                .headers
                .into_iter()
                .map(|h| ilp::plugin::types::HttpHeader {
                    name: h.name,
                    value: h.value,
                })
                .collect(),
            body: request.body,
        };
        let resp = crate::http::send(&request).map_err(|e| format!("{e:#}"))?;
        validate_http_response(&resp)?;
        Ok(legacy::ilp::plugin::host::HttpResponse {
            status: resp.status,
            headers: resp
                .headers
                .into_iter()
                .map(|h| legacy::ilp::plugin::types::HttpHeader {
                    name: h.name,
                    value: h.value,
                })
                .collect(),
            body: resp.body,
        })
    }

    fn storage_get(&mut self, key: String) -> Option<Vec<u8>> {
        <HostState as current::ilp::plugin::host::Host>::storage_get(self, key)
    }

    fn storage_set(&mut self, key: String, value: Vec<u8>) -> Result<(), String> {
        <HostState as current::ilp::plugin::host::Host>::storage_set(self, key, value)
    }

    fn storage_delete(&mut self, key: String) -> Result<(), String> {
        <HostState as current::ilp::plugin::host::Host>::storage_delete(self, key)
    }
}

impl legacy::ilp::plugin::types::Host for HostState {}

impl ilp::plugin::host::HostHttpHandle for HostState {
    fn fetch(
        &mut self,
        _handle: Resource<HttpCapability>,
        request: ilp::plugin::types::HttpRequest,
    ) -> Result<ilp::plugin::types::HttpResponse, String> {
        self.policy
            .allows("http")
            .then_some(())
            .ok_or_else(|| "permission-denied: http".to_string())?;
        crate::http::validate_plugin_url(&request.url).map_err(|e| format!("{e:#}"))?;
        validate_http_request(&request)?;
        self.policy.allows_url(&request.url)?;
        let response = crate::http::send(&request).map_err(|e| format!("{e:#}"))?;
        validate_http_response(&response)?;
        Ok(response)
    }

    fn drop(&mut self, handle: Resource<HttpCapability>) -> wasmtime::Result<()> {
        self.table.delete(handle)?;
        Ok(())
    }
}

impl ilp::plugin::host::HostStorageHandle for HostState {
    fn get(&mut self, _handle: Resource<StorageCapability>, key: String) -> Option<Vec<u8>> {
        <HostState as current::ilp::plugin::host::Host>::storage_get(self, key)
    }

    fn set(
        &mut self,
        _handle: Resource<StorageCapability>,
        key: String,
        value: Vec<u8>,
    ) -> Result<(), String> {
        <HostState as current::ilp::plugin::host::Host>::storage_set(self, key, value)
    }

    fn delete(&mut self, _handle: Resource<StorageCapability>, key: String) -> Result<(), String> {
        <HostState as current::ilp::plugin::host::Host>::storage_delete(self, key)
    }

    fn drop(&mut self, handle: Resource<StorageCapability>) -> wasmtime::Result<()> {
        self.table.delete(handle)?;
        Ok(())
    }
}

impl ilp::plugin::host::HostSecretHandle for HostState {
    fn get(&mut self, _handle: Resource<SecretCapability>, key: String) -> Option<Vec<u8>> {
        if !self.policy.allows("secret-read") || !is_safe_key(&key) {
            return None;
        }
        std::fs::read(self.secret_dir.join(key)).ok()
    }

    fn set(
        &mut self,
        _handle: Resource<SecretCapability>,
        key: String,
        value: Vec<u8>,
    ) -> Result<(), String> {
        if !self.policy.allows("secret-write") {
            return Err("permission-denied: secret-write".to_string());
        }
        if !is_safe_key(&key) {
            return Err("非法 secret key".to_string());
        }
        if value.len() > MAX_STORAGE_VALUE {
            return Err(format!("secret value 过大: {} 字节", value.len()));
        }
        std::fs::create_dir_all(&self.secret_dir).map_err(|e| e.to_string())?;
        let path = self.secret_dir.join(key);
        atomic_write(&path, &value).map_err(|e| e.to_string())?;
        restrict_secret_permissions(&path).map_err(|e| e.to_string())
    }

    fn delete(&mut self, _handle: Resource<SecretCapability>, key: String) -> Result<(), String> {
        if !self.policy.allows("secret-write") || !is_safe_key(&key) {
            return Err("permission-denied: secret-write".to_string());
        }
        match std::fs::remove_file(self.secret_dir.join(key)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }

    fn drop(&mut self, handle: Resource<SecretCapability>) -> wasmtime::Result<()> {
        self.table.delete(handle)?;
        Ok(())
    }
}

fn validate_http_request(request: &ilp::plugin::types::HttpRequest) -> Result<(), String> {
    if request
        .body
        .as_ref()
        .is_some_and(|body| body.len() > MAX_HTTP_BODY)
    {
        return Err(format!("request body 过大: 超过 {MAX_HTTP_BODY} 字节"));
    }
    if request.headers.len() > MAX_HTTP_HEADERS {
        return Err(format!("HTTP header 数量过多: 超过 {MAX_HTTP_HEADERS}"));
    }
    if request
        .headers
        .iter()
        .any(|h| h.name.len() > 256 || h.value.len() > MAX_HTTP_HEADER_VALUE)
    {
        return Err("HTTP header 过大".to_string());
    }
    Ok(())
}

fn validate_legacy_http_request(
    request: &legacy::ilp::plugin::host::HttpRequest,
) -> Result<(), String> {
    if request
        .body
        .as_ref()
        .is_some_and(|body| body.len() > MAX_HTTP_BODY)
    {
        return Err(format!("request body 过大: 超过 {MAX_HTTP_BODY} 字节"));
    }
    if request.headers.len() > MAX_HTTP_HEADERS
        || request
            .headers
            .iter()
            .any(|h| h.name.len() > 256 || h.value.len() > MAX_HTTP_HEADER_VALUE)
    {
        return Err("HTTP header 过大".to_string());
    }
    Ok(())
}

fn validate_http_response(response: &ilp::plugin::types::HttpResponse) -> Result<(), String> {
    if response.body.len() > MAX_HTTP_RESPONSE {
        return Err(format!("HTTP response 过大: 超过 {MAX_HTTP_RESPONSE} 字节"));
    }
    if response.headers.len() > MAX_HTTP_HEADERS
        || response
            .headers
            .iter()
            .any(|h| h.name.len() > 256 || h.value.len() > MAX_HTTP_HEADER_VALUE)
    {
        return Err("HTTP response header 过大".to_string());
    }
    Ok(())
}

fn is_safe_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 128
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !key.starts_with('.')
        && !key.contains("..")
}

/// 写临时文件再 rename：secret 里绝不能出现“写了一半的凭据”。
/// 临时名带纳秒后缀，避免两个不同 key（`x.1` / `x.2`）坍缩到同一个临时文件。
fn atomic_write(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "secret".into());
    let tmp = path.with_file_name(format!(".{name}.{}.{nanos}.tmp", std::process::id()));
    let result = (|| {
        std::fs::write(&tmp, data)?;
        // 先把临时文件收窄到 0600/ACL，再 rename 到正式名，
        // 避免“文件已可见但还没设权限”的窗口期。
        restrict_secret_permissions(&tmp)?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// 把凭据文件限制为仅当前用户可读写。
///
/// Unix 用 `0600`。Windows 默认继承父目录 ACL，可能让同机其他账户读到，
/// 因此显式重写 DACL：先断开继承（`/inheritance:r`），再只授予当前用户。
/// 失败必须上报——静默失败等于留下一个全世界可读的设备密钥。
fn restrict_secret_permissions(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(windows)]
    {
        use std::process::Command;
        let user = std::env::var("USERNAME").map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "无法确定当前 Windows 用户")
        })?;
        let principal = match std::env::var("USERDOMAIN") {
            Ok(domain) if !domain.is_empty() => format!("{domain}\\{user}"),
            _ => user,
        };
        // icacls 是系统自带工具，不需要额外 crate 或链接 Win32 API。
        let status = Command::new("icacls")
            .arg(path)
            .arg("/inheritance:r")
            .arg("/grant:r")
            .arg(format!("{principal}:F"))
            .status()?;
        if !status.success() {
            return Err(std::io::Error::other(format!(
                "设置 secret 文件 ACL 失败（icacls 退出码 {:?}）",
                status.code()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod security_tests {
    use super::*;

    #[test]
    fn domain_matching_respects_label_boundaries() {
        assert!(domain_matches("fanqienovel.com", "fanqienovel.com"));
        assert!(domain_matches("api.fanqienovel.com", "*.fanqienovel.com"));
        assert!(!domain_matches("evilfanqienovel.com", "fanqienovel.com"));
        assert!(!domain_matches(
            "fanqienovel.com.evil.test",
            "fanqienovel.com"
        ));
    }

    #[test]
    fn policy_rejects_http_and_unlisted_domains() {
        let policy = CapabilityPolicy {
            permissions: ["http".to_string()].into_iter().collect(),
            allowed_domains: vec!["fanqienovel.com".to_string()],
        };
        assert!(policy.allows_url("https://fanqienovel.com/page/1").is_ok());
        assert!(policy.allows_url("http://fanqienovel.com/page/1").is_err());
        assert!(policy.allows_url("https://evil.example/page/1").is_err());
    }

    #[test]
    fn undeclared_capabilities_are_not_issued() {
        let mut state = HostState::with_policy(
            "test".to_string(),
            PathBuf::from("/tmp/ilp-test-storage"),
            CapabilityPolicy {
                permissions: std::collections::HashSet::new(),
                allowed_domains: Vec::new(),
            },
        );
        assert!(state.acquire_http().is_none());
        assert!(state.acquire_storage().is_none());
        assert!(state.acquire_secret().is_none());
        assert!(state.storage_set("key".to_string(), vec![1]).is_err());
    }

    #[test]
    fn secrets_are_isolated_and_permission_checked() {
        let root = std::env::temp_dir().join(format!("ilp-secret-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut state = HostState::with_policy_and_secret(
            "plugin-a".to_string(),
            root.join("storage"),
            root.join("secrets/plugin-a"),
            CapabilityPolicy {
                permissions: ["secret-read".to_string(), "secret-write".to_string()]
                    .into_iter()
                    .collect(),
                allowed_domains: Vec::new(),
            },
        );
        <HostState as ilp::plugin::host::HostSecretHandle>::set(
            &mut state,
            Resource::new_own(0),
            "token".into(),
            b"value".to_vec(),
        )
        .expect("写入 secret");
        assert_eq!(
            <HostState as ilp::plugin::host::HostSecretHandle>::get(
                &mut state,
                Resource::new_own(0),
                "token".into()
            ),
            Some(b"value".to_vec())
        );
        assert!(<HostState as ilp::plugin::host::HostSecretHandle>::set(
            &mut state,
            Resource::new_own(0),
            "../escape".into(),
            vec![1]
        )
        .is_err());

        // 凭据文件必须只有当前用户可读（Unix 上是 0600）。
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(root.join("secrets/plugin-a/token"))
                .expect("secret 文件应存在")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "secret 权限应为 0600，实得 {mode:o}");
        }

        // 写入后不能残留临时文件（临时文件同样含明文凭据）。
        let leftovers: Vec<String> = std::fs::read_dir(root.join("secrets/plugin-a"))
            .expect("读 secret 目录")
            .map(|e| e.expect("条目").file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "不应残留临时文件: {leftovers:?}");

        let _ = std::fs::remove_dir_all(&root);
    }
}

/// 已加载的插件。
pub struct PluginInstance {
    pub metadata: PluginMetadata,
    backend: PluginBackend,
}

enum PluginBackend {
    Current {
        store: Store<HostState>,
        bindings: current::NovelPlugin,
    },
    Legacy {
        store: Store<HostState>,
        bindings: legacy::NovelPlugin,
    },
}

impl PluginInstance {
    pub fn load(path: &Path, storage_root: &Path) -> Result<Self> {
        let mut engine_config = WasmtimeConfig::new();
        engine_config.consume_fuel(true);
        let engine =
            Engine::new(&engine_config).map_err(|e| anyhow!("创建 Wasmtime engine 失败: {e}"))?;
        let component = Component::from_file(&engine, path)
            .map_err(|e| anyhow!("读取组件失败 {}: {e}", path.display()))?;
        let version = component
            .component_type()
            .exports(&engine)
            .find_map(|(name, _)| name.strip_prefix("ilp:plugin/plugin@").map(str::to_string))
            .ok_or_else(|| anyhow!("组件 {} 没有导出 ilp:plugin/plugin", path.display()))?;
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("plugin")
            .to_string();
        match version.as_str() {
            "0.2.0" => {
                let mut linker = Linker::new(&engine);
                wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
                    .map_err(|e| anyhow!("注册 WASI p2 失败: {e}"))?;
                current::NovelPlugin::add_to_linker::<
                    HostState,
                    wasmtime::component::HasSelf<HostState>,
                >(&mut linker, |s| s)?;
                let mut store = Store::new(
                    &engine,
                    HostState::new(name.clone(), storage_root.join(&name)),
                );
                store.limiter(|state| &mut state.limits);
                store
                    .set_fuel(MAX_PLUGIN_FUEL)
                    .map_err(|e| anyhow!("设置插件 fuel 失败: {e}"))?;
                let bindings = current::NovelPlugin::instantiate(&mut store, &component, &linker)
                    .map_err(|e| anyhow!("实例化插件失败 {}: {e}", path.display()))?;
                let raw = bindings
                    .ilp_plugin_plugin()
                    .call_metadata(&mut store)
                    .map_err(|e| anyhow!("调用插件 metadata 失败: {e}"))?;
                let metadata = PluginMetadata {
                    abi_version: raw.abi_version,
                    site_name: raw.site_name,
                    site_id: raw.site_id,
                    site_domain: raw.site_domain,
                    plugin_name: raw.plugin_name,
                    plugin_id: raw.plugin_id,
                    version: raw.version,
                    permissions: raw.permissions.into_iter().map(permission_name).collect(),
                    allowed_domains: raw.allowed_domains,
                };
                store.data_mut().policy = CapabilityPolicy {
                    permissions: metadata.permissions.iter().cloned().collect(),
                    allowed_domains: metadata.allowed_domains.clone(),
                };
                Self::finish(metadata, PluginBackend::Current { store, bindings })
            }
            "0.1.0" => {
                let mut linker = Linker::new(&engine);
                wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
                    .map_err(|e| anyhow!("注册 WASI p2 失败: {e}"))?;
                legacy::NovelPlugin::add_to_linker::<
                    HostState,
                    wasmtime::component::HasSelf<HostState>,
                >(&mut linker, |s| s)?;
                let mut store = Store::new(
                    &engine,
                    HostState::new(name.clone(), storage_root.join(&name)),
                );
                store.limiter(|state| &mut state.limits);
                store
                    .set_fuel(MAX_PLUGIN_FUEL)
                    .map_err(|e| anyhow!("设置插件 fuel 失败: {e}"))?;
                let bindings = legacy::NovelPlugin::instantiate(&mut store, &component, &linker)
                    .map_err(|e| anyhow!("实例化旧版插件失败 {}: {e}", path.display()))?;
                let raw = bindings
                    .ilp_plugin_plugin()
                    .call_metadata(&mut store)
                    .map_err(|e| anyhow!("调用旧版插件 metadata 失败: {e}"))?;
                let metadata = PluginMetadata {
                    abi_version: raw.abi_version,
                    site_name: raw.site_name,
                    site_id: raw.site_id,
                    site_domain: raw.site_domain,
                    plugin_name: raw.plugin_name,
                    plugin_id: raw.plugin_id,
                    version: raw.version,
                    permissions: Vec::new(),
                    allowed_domains: Vec::new(),
                };
                Self::finish(metadata, PluginBackend::Legacy { store, bindings })
            }
            other => Err(anyhow!("插件 ABI 版本 {other} 不受支持")),
        }
    }

    fn finish(metadata: PluginMetadata, backend: PluginBackend) -> Result<Self> {
        if metadata.abi_version != "0.1.0" && metadata.abi_version != "0.2.0" {
            return Err(anyhow!(
                "插件 {} 的 ABI 版本 {} 不受支持",
                metadata.plugin_name,
                metadata.abi_version
            ));
        }
        Ok(Self { metadata, backend })
    }

    pub fn site_id(&self) -> &str {
        &self.metadata.site_id
    }

    pub fn init(&mut self, book_id: &str, force_update: bool) -> Result<()> {
        match &mut self.backend {
            PluginBackend::Current { store, bindings } => bindings
                .ilp_plugin_plugin()
                .call_init(store, book_id, force_update)
                .map_err(|e| anyhow!("plugin::{e}"))?
                .map_err(|e| anyhow!("plugin::{e}")),
            PluginBackend::Legacy { store, bindings } => bindings
                .ilp_plugin_plugin()
                .call_init(store, book_id, force_update)
                .map_err(|e| anyhow!("plugin::{e}"))?
                .map_err(|e| anyhow!("plugin::{e}")),
        }
    }

    pub fn title(&mut self) -> Result<String> {
        match &mut self.backend {
            PluginBackend::Current { store, bindings } => bindings
                .ilp_plugin_plugin()
                .call_title(store)
                .map_err(|e| anyhow!("plugin::{e}"))?
                .map_err(|e| anyhow!("plugin::{e}")),
            PluginBackend::Legacy { store, bindings } => bindings
                .ilp_plugin_plugin()
                .call_title(store)
                .map_err(|e| anyhow!("plugin::{e}"))?
                .map_err(|e| anyhow!("plugin::{e}")),
        }
    }

    pub fn author(&mut self) -> Result<String> {
        match &mut self.backend {
            PluginBackend::Current { store, bindings } => bindings
                .ilp_plugin_plugin()
                .call_author(store)
                .map_err(|e| anyhow!("plugin::{e}"))?
                .map_err(|e| anyhow!("plugin::{e}")),
            PluginBackend::Legacy { store, bindings } => bindings
                .ilp_plugin_plugin()
                .call_author(store)
                .map_err(|e| anyhow!("plugin::{e}"))?
                .map_err(|e| anyhow!("plugin::{e}")),
        }
    }

    pub fn cover_url(&mut self) -> Result<String> {
        match &mut self.backend {
            PluginBackend::Current { store, bindings } => bindings
                .ilp_plugin_plugin()
                .call_cover_url(store)
                .map_err(|e| anyhow!("plugin::{e}"))?
                .map_err(|e| anyhow!("plugin::{e}")),
            PluginBackend::Legacy { store, bindings } => bindings
                .ilp_plugin_plugin()
                .call_cover_url(store)
                .map_err(|e| anyhow!("plugin::{e}"))?
                .map_err(|e| anyhow!("plugin::{e}")),
        }
    }

    pub fn description(&mut self) -> Result<String> {
        match &mut self.backend {
            PluginBackend::Current { store, bindings } => bindings
                .ilp_plugin_plugin()
                .call_description(store)
                .map_err(|e| anyhow!("plugin::{e}"))?
                .map_err(|e| anyhow!("plugin::{e}")),
            PluginBackend::Legacy { store, bindings } => bindings
                .ilp_plugin_plugin()
                .call_description(store)
                .map_err(|e| anyhow!("plugin::{e}"))?
                .map_err(|e| anyhow!("plugin::{e}")),
        }
    }

    pub fn catalog(&mut self, force: bool) -> Result<Vec<ChapterEntry>> {
        match &mut self.backend {
            PluginBackend::Current { store, bindings } => Ok(bindings
                .ilp_plugin_plugin()
                .call_catalog(store, force)
                .map_err(|e| anyhow!("plugin::{e}"))?
                .map_err(|e| anyhow!("plugin::{e}"))?
                .into_iter()
                .map(convert_entry)
                .collect()),
            PluginBackend::Legacy { store, bindings } => Ok(bindings
                .ilp_plugin_plugin()
                .call_catalog(store, force)
                .map_err(|e| anyhow!("plugin::{e}"))?
                .map_err(|e| anyhow!("plugin::{e}"))?
                .into_iter()
                .map(convert_legacy_entry)
                .collect()),
        }
    }

    pub fn search(&mut self, keyword: &str, cursor: &str) -> Result<ilp_types::SearchPage> {
        match &mut self.backend {
            PluginBackend::Current { store, bindings } => convert_search(
                bindings
                    .ilp_plugin_plugin()
                    .call_search(store, keyword, cursor)
                    .map_err(|e| anyhow!("plugin::{e}"))?
                    .map_err(|e| anyhow!("plugin::{e}"))?,
            ),
            PluginBackend::Legacy { store, bindings } => convert_legacy_search(
                bindings
                    .ilp_plugin_plugin()
                    .call_search(store, keyword, cursor)
                    .map_err(|e| anyhow!("plugin::{e}"))?
                    .map_err(|e| anyhow!("plugin::{e}"))?,
            ),
        }
    }

    pub fn plan_fetch(&mut self, ids: &[String]) -> Result<Vec<FetchTask>> {
        if ids.len() > MAX_FETCH_TASKS {
            return Err(anyhow!(
                "抓取任务过多: {}，上限 {MAX_FETCH_TASKS}",
                ids.len()
            ));
        }
        match &mut self.backend {
            PluginBackend::Current { store, bindings } => Ok(bindings
                .ilp_plugin_plugin()
                .call_plan_fetch(store, ids)
                .map_err(|e| anyhow!("plugin::{e}"))?
                .map_err(|e| anyhow!("plugin::{e}"))?),
            PluginBackend::Legacy { store, bindings } => Ok(bindings
                .ilp_plugin_plugin()
                .call_plan_fetch(store, ids)
                .map_err(|e| anyhow!("plugin::{e}"))?
                .map_err(|e| anyhow!("plugin::{e}"))?
                .into_iter()
                .map(convert_legacy_task)
                .collect()),
        }
    }

    pub fn consume_fetch(
        &mut self,
        task_id: &str,
        resp: HttpResponse,
    ) -> Result<Vec<ChapterContent>> {
        match &mut self.backend {
            PluginBackend::Current { store, bindings } => Ok(bindings
                .ilp_plugin_plugin()
                .call_consume_fetch(store, task_id, &resp)
                .map_err(|e| anyhow!("plugin::{e}"))?
                .map_err(|e| anyhow!("plugin::{e}"))?
                .into_iter()
                .filter_map(|c| {
                    c.text.map(|text| ChapterContent {
                        id: c.id,
                        title: c.title,
                        text,
                    })
                })
                .collect()),
            PluginBackend::Legacy { store, bindings } => {
                let resp = convert_legacy_response(resp);
                Ok(bindings
                    .ilp_plugin_plugin()
                    .call_consume_fetch(store, task_id, &resp)
                    .map_err(|e| anyhow!("plugin::{e}"))?
                    .map_err(|e| anyhow!("plugin::{e}"))?
                    .into_iter()
                    .map(|c| ChapterContent {
                        id: c.id,
                        title: c.title,
                        text: c.text,
                    })
                    .collect())
            }
        }
    }
}

fn convert_entry(e: ilp::plugin::types::ChapterEntry) -> ChapterEntry {
    ChapterEntry {
        id: e.id,
        title: e.title,
        url: e.url,
        fetch_url: e.fetch_url,
        update_time: e.update_time,
    }
}

fn convert_legacy_entry(e: legacy::ilp::plugin::types::ChapterEntry) -> ChapterEntry {
    ChapterEntry {
        id: e.id,
        title: e.title,
        url: e.url,
        fetch_url: e.fetch_url,
        update_time: e.update_time,
    }
}

fn convert_search(raw: ilp::plugin::types::SearchPage) -> Result<ilp_types::SearchPage> {
    Ok(ilp_types::SearchPage {
        cursor: raw.cursor,
        has_more: raw.has_more,
        results: raw
            .results
            .into_iter()
            .map(|r| SearchResult {
                id: r.id,
                title: r.title,
                author: r.author,
                cover_url: r.cover_url,
                description: r.description,
            })
            .collect(),
    })
}

fn convert_legacy_search(
    raw: legacy::ilp::plugin::types::SearchPage,
) -> Result<ilp_types::SearchPage> {
    Ok(ilp_types::SearchPage {
        cursor: raw.cursor,
        has_more: raw.has_more,
        results: raw
            .results
            .into_iter()
            .map(|r| SearchResult {
                id: r.id,
                title: r.title,
                author: r.author,
                cover_url: r.cover_url,
                description: r.description,
            })
            .collect(),
    })
}

fn convert_legacy_task(t: legacy::ilp::plugin::types::FetchTask) -> ilp::plugin::types::FetchTask {
    ilp::plugin::types::FetchTask {
        id: t.id,
        request: ilp::plugin::types::HttpRequest {
            method: t.request.method,
            url: t.request.url,
            headers: t
                .request
                .headers
                .into_iter()
                .map(|h| ilp::plugin::types::HttpHeader {
                    name: h.name,
                    value: h.value,
                })
                .collect(),
            body: t.request.body,
        },
    }
}

fn convert_legacy_response(r: HttpResponse) -> legacy::ilp::plugin::types::HttpResponse {
    legacy::ilp::plugin::types::HttpResponse {
        status: r.status,
        headers: r
            .headers
            .into_iter()
            .map(|h| legacy::ilp::plugin::types::HttpHeader {
                name: h.name,
                value: h.value,
            })
            .collect(),
        body: r.body,
    }
}

/// 加载目录下所有插件，单个失败只告警不中断。
pub fn load_plugins(config: &crate::config::Config) -> Result<Vec<PluginInstance>> {
    let dir = config.plugins_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Ok(Vec::new()),
    };

    let storage_root = Path::new(&config.data_folder).join("plugins");
    let mut out = Vec::new();
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("wasm"))
        .collect();
    paths.sort();

    // 记下每个插件来自哪个文件：冲突时报文件名才可操作，
    // 只报 plugin_name 时两个同名插件看上去一模一样。
    let mut origins: Vec<(PathBuf, String)> = Vec::new();
    for path in paths {
        match PluginInstance::load(&path, &storage_root) {
            Ok(p) => {
                origins.push((path.clone(), p.site_id().to_string()));
                out.push(p);
            }
            Err(e) => tracing::warn!("加载插件 {} 失败: {e:#}", path.display()),
        }
    }

    // 插件按 site_id 查找，而这里按文件名排序加载，所以两个插件声明同一个
    // site_id 时，`ilp get -s <site>` 用哪个取决于文件名——同一个命令在不同
    // 机器上行为不同。用户看不出原因，只会觉得"有时候抓错了"。
    warn_on_duplicate_site_ids(&origins);

    Ok(out)
}

/// 报告重复的 site_id。只警告不拒绝：插件本身是好的，
/// 问题在于用户同时装了两个抢同一个站点的插件。
///
/// `origins` 是 (文件路径, site_id)。
fn warn_on_duplicate_site_ids(origins: &[(PathBuf, String)]) {
    let mut by_site: std::collections::BTreeMap<&str, Vec<&Path>> =
        std::collections::BTreeMap::new();
    for (path, site_id) in origins {
        by_site.entry(site_id.as_str()).or_default().push(path);
    }
    for (site_id, paths) in by_site.iter().filter(|(_, p)| p.len() > 1) {
        let files: Vec<String> = paths
            .iter()
            .map(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| p.display().to_string())
            })
            .collect();
        tracing::warn!(
            "site_id {site_id} 被多个文件声明: {}。按 site_id 查找时只有其中一个生效，\
             请卸载多余的插件（ilp plugin uninstall <name>）",
            files.join(", ")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_site_ids_are_grouped_by_file() {
        // 两条路径抢同一个 site_id。分组逻辑必须把它们归到一起，
        // 否则告警里只会看到一个文件，用户不知道冲突的是谁。
        let origins = vec![
            (PathBuf::from("/p/aaa.wasm"), "fanqie".to_string()),
            (PathBuf::from("/p/zzz.wasm"), "fanqie".to_string()),
            (PathBuf::from("/p/other.wasm"), "other".to_string()),
        ];
        let mut by_site: std::collections::BTreeMap<&str, Vec<&Path>> =
            std::collections::BTreeMap::new();
        for (path, site_id) in &origins {
            by_site.entry(site_id.as_str()).or_default().push(path);
        }
        let dups: Vec<_> = by_site.iter().filter(|(_, p)| p.len() > 1).collect();
        assert_eq!(dups.len(), 1, "只该报一组重复");
        assert_eq!(dups[0].0, &"fanqie");
        assert_eq!(dups[0].1.len(), 2);
    }

    #[test]
    fn storage_keys_reject_traversal() {
        assert!(is_safe_key("device"));
        assert!(is_safe_key("device_v1.json"));
        assert!(!is_safe_key(""));
        assert!(!is_safe_key("../escape"));
        assert!(!is_safe_key("a/b"));
        assert!(!is_safe_key(".hidden"));
        assert!(!is_safe_key("a..b"));
    }

    #[test]
    fn atomic_write_replaces_content() {
        let dir = std::env::temp_dir().join("ilp_engine_test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("k");
        atomic_write(&p, b"first").unwrap();
        atomic_write(&p, b"second").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"second");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
