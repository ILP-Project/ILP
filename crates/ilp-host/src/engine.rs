use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use wasmtime::component::{Component, Linker, Resource};
use wasmtime::{
    Config as WasmtimeConfig, Engine, Store, StoreContextMut, StoreLimits, StoreLimitsBuilder,
    UpdateDeadline,
};
use wasmtime_wasi::{ResourceTable, WasiCtx, WasiCtxView, WasiView};

pub struct HttpCapability;
pub struct StorageCapability;
pub struct SecretCapability;

const MAX_PLUGIN_MEMORY: usize = 64 * 1024 * 1024;
/// 单次插件调用的算力预算。
///
/// 这是“每次调用”的量，不是进程总量。实测一次 `consume-fetch` 解密 30 章
/// 峰值约 1.7 亿条指令（番茄的 AES + gzip），所以留约 6 倍余量。
/// 定得太小会在正常批量解密时误杀，而一次 trap 之后组件实例不可再用，
/// 表现为后续调用全部报 “cannot enter component instance”。
const MAX_PLUGIN_FUEL: u64 = 1_000_000_000;
/// 墙钟预算：单次插件调用里**真正在跑 WebAssembly** 的时间上限。
/// 宿主函数里的等待（网络、磁盘）不计入——否则一次 30 秒的 HTTP 重试
/// 就能把一个规规矩矩的插件判成超时。
const MAX_PLUGIN_CALL: Duration = Duration::from_secs(60);
/// epoch 推进间隔，决定超时判定的粒度。
const EPOCH_TICK: Duration = Duration::from_millis(100);
const MAX_HTTP_BODY: usize = 2 * 1024 * 1024;
const MAX_HTTP_RESPONSE: usize = 16 * 1024 * 1024;
const MAX_HTTP_HEADERS: usize = 128;
const MAX_HTTP_HEADER_VALUE: usize = 16 * 1024;
const MAX_FETCH_TASKS: usize = 128;
const MAX_STORAGE_VALUE: usize = 4 * 1024 * 1024;
const MAX_LOG_MESSAGE: usize = 16 * 1024;

/// 单次插件调用的时间预算是否已经耗尽。
/// 只看 WebAssembly 自己的执行时间，减掉宿主函数阻塞的部分。
fn call_budget_exhausted(
    started: Instant,
    host_wait: Duration,
    now: Instant,
    limit: Duration,
) -> bool {
    now.duration_since(started)
        .saturating_sub(host_wait)
        .gt(&limit)
}

/// 把时间预算换算成 epoch 刻度数（向上取整，避免 0 预算被判为无限）。
fn deadline_ticks(limit: Duration) -> u64 {
    let tick = EPOCH_TICK.as_millis().max(1);
    let ms = limit.as_millis();
    let ticks = ms.div_ceil(tick);
    u64::try_from(ticks).unwrap_or(u64::MAX)
}

/// 进程内共享的 Wasmtime engine。
///
/// 每次加载插件都新建 engine 会重复编译期开销、重复分配线程池，也会让
/// “全局资源上限”变成“每实例上限”。engine 无状态，共享是安全的。
fn shared_engine() -> &'static Engine {
    static ENGINE: OnceLock<Engine> = OnceLock::new();
    ENGINE.get_or_init(|| {
        let mut config = WasmtimeConfig::new();
        config.consume_fuel(true);
        // epoch 只能被外部递增，它是墙钟时间进入沙箱的唯一入口。
        config.epoch_interruption(true);
        let engine = Engine::new(&config).expect("创建 Wasmtime engine 失败");
        let ticker = engine.clone();
        std::thread::Builder::new()
            .name("ilp-epoch".into())
            .spawn(move || loop {
                std::thread::sleep(EPOCH_TICK);
                ticker.increment_epoch();
            })
            .expect("启动 epoch 线程失败");
        engine
    })
}

/// epoch 到期时的判定。
///
/// 到期只证明「墙钟时间过了」，还需要再减掉宿主函数阻塞的时长，
/// 否则一次正常重试的 30 秒 HTTP 就会被当成插件死循环。
fn epoch_deadline(mut cx: StoreContextMut<'_, HostState>) -> wasmtime::Result<UpdateDeadline> {
    let now = Instant::now();
    let state = cx.data_mut();
    // 用户按了 Ctrl-C：正在沙箱里跑的插件不该继续算下去。
    // 检查点在 epoch 上，粒度就是一个 EPOCH_TICK。
    if crate::cancel::requested() {
        state.interrupted = true;
        return Ok(UpdateDeadline::Interrupt);
    }
    if call_budget_exhausted(state.call_started, state.host_wait, now, state.call_limit) {
        state.timed_out = true;
        return Ok(UpdateDeadline::Interrupt);
    }
    let spent = now
        .duration_since(state.call_started)
        .saturating_sub(state.host_wait);
    let remaining = state.call_limit.saturating_sub(spent);
    // 至少推进 1 个 tick：返回 0 会让 deadline 永远停在当前时刻，
    // 之后每次检查都立即到期。
    Ok(UpdateDeadline::Continue(deadline_ticks(remaining).max(1)))
}

/// 建立一个带资源上限与墙钟预算的 store。
fn new_store(engine: &Engine, state: HostState) -> Result<Store<HostState>> {
    let mut store = Store::new(engine, state);
    store.limiter(|state| &mut state.limits);
    store
        .set_fuel(MAX_PLUGIN_FUEL)
        .map_err(|e| anyhow!("设置插件 fuel 失败: {e}"))?;
    store.epoch_deadline_callback(epoch_deadline);
    arm_deadline(&mut store);
    Ok(store)
}

/// 每次进入插件调用前重置预算。
/// 不重置的话，第二次调用的起点还是加载时刻，插件用得越久越容易被误杀。
fn arm_deadline(store: &mut Store<HostState>) {
    let limit = store.data().call_limit;
    let state = store.data_mut();
    state.call_started = Instant::now();
    state.host_wait = Duration::ZERO;
    state.timed_out = false;
    state.interrupted = false;
    store.set_epoch_deadline(deadline_ticks(limit).max(1));
    // fuel 也是“每次调用”的预算，不是整个进程的总量。
    // 不补充的话，第一个大章节批次就会烧完燃料，之后每次调用都直接
    // trap 成 “cannot enter component instance”。
    let budget = store.data().fuel_budget;
    if let Err(e) = store.set_fuel(budget) {
        tracing::warn!("补充插件 fuel 失败: {e}");
    }
}

/// 把 wasm trap 翻译成人能读的错误。
///
/// 超时、取消、算力耗尽和插件自己报错是四件不同的事：都是“运行没成功”，
/// 但原因与处置方式完全不同。混在一起会让用户去查一个根本不存在的 bug。
fn guest_error(store: &Store<HostState>, e: wasmtime::Error) -> anyhow::Error {
    if store.data().interrupted || crate::cancel::requested() {
        anyhow::Error::new(crate::cancel::Cancelled)
    } else if store.data().timed_out {
        anyhow!(
            "plugin::timeout: 插件执行超过 {} 秒被中止",
            store.data().call_limit.as_secs()
        )
    } else if format!("{e:?}").contains("cannot enter component instance") {
        anyhow!("plugin::poisoned: 插件实例因上一次调用中止而不可再用，请重试")
    } else if store.get_fuel().is_ok_and(|f| f == 0) {
        anyhow!(
            "plugin::fuel: 单次调用耗尽算力预算（{} 条指令），已中止",
            MAX_PLUGIN_FUEL
        )
    } else {
        anyhow!("plugin::{e:?}")
    }
}

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
    /// 本次插件调用的起点，epoch 回调用它算墙钟预算。
    call_started: Instant,
    /// 本次调用里宿主函数（网络、磁盘）已经消耗的时间，不计入预算。
    host_wait: Duration,
    call_limit: Duration,
    /// 单次调用可用的 fuel。测试需要把 fuel 让位给墙钟时改成 u64::MAX，
    /// 否则 `arm_deadline` 每次调用都会把它补回默认值，测试前提被悄悄推翻。
    fuel_budget: u64,
    /// epoch 回调因预算耗尽而中断过当前调用。
    timed_out: bool,
    /// epoch 回调因用户取消而中断过当前调用。
    interrupted: bool,
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
            call_started: Instant::now(),
            host_wait: Duration::ZERO,
            call_limit: MAX_PLUGIN_CALL,
            fuel_budget: MAX_PLUGIN_FUEL,
            timed_out: false,
            interrupted: false,
        }
    }

    /// 把一次阻塞的宿主操作从墙钟预算里扣除。
    /// 前提：宿主函数里不会重入 wasm，因此调用期间的耗时全部属于宿主。
    fn time_host_call<T>(&mut self, f: impl FnOnce() -> T) -> T {
        let started = Instant::now();
        let out = f();
        self.host_wait += started.elapsed();
        out
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
        let response = self
            .time_host_call(|| crate::http::send(&request))
            .map_err(|e| format!("{e:#}"))?;
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
        let resp = self
            .time_host_call(|| crate::http::send(&request))
            .map_err(|e| format!("{e:#}"))?;
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
        let response = self
            .time_host_call(|| crate::http::send(&request))
            .map_err(|e| format!("{e:#}"))?;
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
        let engine = shared_engine();
        let component = Component::from_file(engine, path)
            .map_err(|e| anyhow!("读取组件失败 {}: {e}", path.display()))?;
        let version = component
            .component_type()
            .exports(engine)
            .find_map(|(name, _)| name.strip_prefix("ilp:plugin/plugin@").map(str::to_string))
            .ok_or_else(|| anyhow!("组件 {} 没有导出 ilp:plugin/plugin", path.display()))?;
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("plugin")
            .to_string();
        match version.as_str() {
            "0.2.0" => {
                let mut linker = Linker::new(engine);
                wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
                    .map_err(|e| anyhow!("注册 WASI p2 失败: {e}"))?;
                current::NovelPlugin::add_to_linker::<
                    HostState,
                    wasmtime::component::HasSelf<HostState>,
                >(&mut linker, |s| s)?;
                let mut store = new_store(
                    engine,
                    HostState::new(name.clone(), storage_root.join(&name)),
                )?;
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
                let mut linker = Linker::new(engine);
                wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
                    .map_err(|e| anyhow!("注册 WASI p2 失败: {e}"))?;
                legacy::NovelPlugin::add_to_linker::<
                    HostState,
                    wasmtime::component::HasSelf<HostState>,
                >(&mut linker, |s| s)?;
                let mut store = new_store(
                    engine,
                    HostState::new(name.clone(), storage_root.join(&name)),
                )?;
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
            PluginBackend::Current { store, bindings } => guest_call(store, |s| {
                bindings
                    .ilp_plugin_plugin()
                    .call_init(s, book_id, force_update)
            })?
            .map_err(|e| anyhow!("plugin::{e}")),
            PluginBackend::Legacy { store, bindings } => guest_call(store, |s| {
                bindings
                    .ilp_plugin_plugin()
                    .call_init(s, book_id, force_update)
            })?
            .map_err(|e| anyhow!("plugin::{e}")),
        }
    }

    pub fn title(&mut self) -> Result<String> {
        match &mut self.backend {
            PluginBackend::Current { store, bindings } => {
                guest_call(store, |s| bindings.ilp_plugin_plugin().call_title(s))?
                    .map_err(|e| anyhow!("plugin::{e}"))
            }
            PluginBackend::Legacy { store, bindings } => {
                guest_call(store, |s| bindings.ilp_plugin_plugin().call_title(s))?
                    .map_err(|e| anyhow!("plugin::{e}"))
            }
        }
    }

    pub fn author(&mut self) -> Result<String> {
        match &mut self.backend {
            PluginBackend::Current { store, bindings } => {
                guest_call(store, |s| bindings.ilp_plugin_plugin().call_author(s))?
                    .map_err(|e| anyhow!("plugin::{e}"))
            }
            PluginBackend::Legacy { store, bindings } => {
                guest_call(store, |s| bindings.ilp_plugin_plugin().call_author(s))?
                    .map_err(|e| anyhow!("plugin::{e}"))
            }
        }
    }

    pub fn cover_url(&mut self) -> Result<String> {
        match &mut self.backend {
            PluginBackend::Current { store, bindings } => {
                guest_call(store, |s| bindings.ilp_plugin_plugin().call_cover_url(s))?
                    .map_err(|e| anyhow!("plugin::{e}"))
            }
            PluginBackend::Legacy { store, bindings } => {
                guest_call(store, |s| bindings.ilp_plugin_plugin().call_cover_url(s))?
                    .map_err(|e| anyhow!("plugin::{e}"))
            }
        }
    }

    pub fn description(&mut self) -> Result<String> {
        match &mut self.backend {
            PluginBackend::Current { store, bindings } => {
                guest_call(store, |s| bindings.ilp_plugin_plugin().call_description(s))?
                    .map_err(|e| anyhow!("plugin::{e}"))
            }
            PluginBackend::Legacy { store, bindings } => {
                guest_call(store, |s| bindings.ilp_plugin_plugin().call_description(s))?
                    .map_err(|e| anyhow!("plugin::{e}"))
            }
        }
    }

    pub fn catalog(&mut self, force: bool) -> Result<Vec<ChapterEntry>> {
        match &mut self.backend {
            PluginBackend::Current { store, bindings } => Ok(guest_call(store, |s| {
                bindings.ilp_plugin_plugin().call_catalog(s, force)
            })?
            .map_err(|e| anyhow!("plugin::{e}"))?
            .into_iter()
            .map(convert_entry)
            .collect()),
            PluginBackend::Legacy { store, bindings } => Ok(guest_call(store, |s| {
                bindings.ilp_plugin_plugin().call_catalog(s, force)
            })?
            .map_err(|e| anyhow!("plugin::{e}"))?
            .into_iter()
            .map(convert_legacy_entry)
            .collect()),
        }
    }

    pub fn search(&mut self, keyword: &str, cursor: &str) -> Result<ilp_types::SearchPage> {
        match &mut self.backend {
            PluginBackend::Current { store, bindings } => convert_search(
                guest_call(store, |s| {
                    bindings.ilp_plugin_plugin().call_search(s, keyword, cursor)
                })?
                .map_err(|e| anyhow!("plugin::{e}"))?,
            ),
            PluginBackend::Legacy { store, bindings } => convert_legacy_search(
                guest_call(store, |s| {
                    bindings.ilp_plugin_plugin().call_search(s, keyword, cursor)
                })?
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
            PluginBackend::Current { store, bindings } => Ok(guest_call(store, |s| {
                bindings.ilp_plugin_plugin().call_plan_fetch(s, ids)
            })?
            .map_err(|e| anyhow!("plugin::{e}"))?),
            PluginBackend::Legacy { store, bindings } => Ok(guest_call(store, |s| {
                bindings.ilp_plugin_plugin().call_plan_fetch(s, ids)
            })?
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
            PluginBackend::Current { store, bindings } => Ok(guest_call(store, |s| {
                bindings
                    .ilp_plugin_plugin()
                    .call_consume_fetch(s, task_id, &resp)
            })?
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
                Ok(guest_call(store, |s| {
                    bindings
                        .ilp_plugin_plugin()
                        .call_consume_fetch(s, task_id, &resp)
                })?
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

/// 进入一次插件调用：重置墙钟预算，并把宿主主动中断和插件自报错误分开。
fn guest_call<T>(
    store: &mut Store<HostState>,
    f: impl FnOnce(&mut Store<HostState>) -> wasmtime::Result<T>,
) -> Result<T> {
    arm_deadline(store);
    let out = f(store);
    match out {
        Ok(v) => Ok(v),
        Err(e) => Err(guest_error(store, e)),
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

    #[test]
    fn deadline_ticks_round_up_and_never_reach_zero() {
        assert_eq!(deadline_ticks(Duration::ZERO), 0);
        assert_eq!(deadline_ticks(Duration::from_millis(1)), 1);
        assert_eq!(deadline_ticks(Duration::from_millis(100)), 1);
        assert_eq!(deadline_ticks(Duration::from_millis(101)), 2);
        assert_eq!(deadline_ticks(MAX_PLUGIN_CALL), 600);
    }

    #[test]
    fn host_wait_does_not_consume_the_call_budget() {
        // 插件里合法的重试会花掉几十秒网络时间。把宿主阻塞算进预算，
        // 一个规规矩矩的插件就会在慢网下被判成死循环。
        let started = Instant::now();
        let now = started + Duration::from_secs(40);
        let limit = Duration::from_secs(10);
        assert!(call_budget_exhausted(started, Duration::ZERO, now, limit));
        assert!(!call_budget_exhausted(
            started,
            Duration::from_secs(35),
            now,
            limit
        ));
    }

    #[test]
    fn call_budget_is_rearmed_from_the_declared_fuel_budget() {
        // 这条测试钉住的是不变量本身，不依赖机器快慢：
        // `arm_deadline` 必须按 `fuel_budget` 补充燃料。之前它写死成
        // MAX_PLUGIN_FUEL，把"测试想关掉 fuel"的意图悄悄覆盖掉，
        // 于是死循环测试在 fuel 与墙钟之间形成竞速，偶发断言失败。
        let engine = shared_engine();
        let storage = std::env::temp_dir().join(format!("ilp-fuel-{}", std::process::id()));
        let mut store = new_store(engine, HostState::new("fuel".to_string(), storage)).unwrap();
        store.data_mut().fuel_budget = 7;
        arm_deadline(&mut store);
        assert_eq!(store.get_fuel().unwrap(), 7);
    }

    #[test]
    fn infinite_loop_is_interrupted_and_reported_as_timeout() {
        let engine = shared_engine();
        let module =
            wasmtime::Module::new(engine, r#"(module (func (export "spin") (loop $l br $l)))"#)
                .expect("编译死循环模块");
        let storage = std::env::temp_dir().join(format!("ilp-spin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&storage);
        let mut store =
            new_store(engine, HostState::new("spin".to_string(), storage.clone())).unwrap();
        // 关掉 fuel，否则先撞 fuel 而不是墙钟。
        //
        // 必须改 `fuel_budget` 而不是直接 `set_fuel`：`guest_call` 会先调
        // `arm_deadline`，那里把 fuel 补回到预算值。只 `set_fuel` 的话，
        // 死循环烧掉 1e9 指令所需的时间与 300ms 墙钟相当，两者形成竞速——
        // 机器繁忙时 fuel 先耗尽，`timed_out` 就不会置位，这个断言随机失败。
        store.data_mut().fuel_budget = u64::MAX;
        store.set_fuel(u64::MAX).unwrap();
        store.data_mut().call_limit = Duration::from_millis(300);
        let instance = wasmtime::Instance::new(&mut store, &module, &[]).expect("实例化");
        let spin = instance
            .get_typed_func::<(), ()>(&mut store, "spin")
            .expect("取 spin");

        let started = Instant::now();
        let err = guest_call(&mut store, |s| spin.call(s, ())).expect_err("死循环必须被中断");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "中断得太慢: {:?}",
            started.elapsed()
        );
        assert!(store.data().timed_out, "应标记为超时而非插件错误");
        assert!(
            err.to_string().contains("timeout"),
            "错误信息应标明超时: {err}"
        );

        let _ = std::fs::remove_dir_all(&storage);
    }

    #[test]
    fn normal_call_does_not_trip_the_deadline() {
        let engine = shared_engine();
        let module = wasmtime::Module::new(engine, r#"(module (func (export "noop")))"#)
            .expect("编译空模块");
        let storage = std::env::temp_dir().join(format!("ilp-noop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&storage);
        let mut store =
            new_store(engine, HostState::new("noop".to_string(), storage.clone())).unwrap();
        let instance = wasmtime::Instance::new(&mut store, &module, &[]).expect("实例化");
        let noop = instance
            .get_typed_func::<(), ()>(&mut store, "noop")
            .expect("取 noop");

        guest_call(&mut store, |s| noop.call(s, ())).expect("正常调用不应超时");
        assert!(!store.data().timed_out);
        // 预算必须被重新武装，否则第二次调用一进去就超时。
        store.data_mut().timed_out = true;
        guest_call(&mut store, |s| noop.call(s, ())).expect("第二次调用不应超时");
        assert!(!store.data().timed_out);

        let _ = std::fs::remove_dir_all(&storage);
    }

    /// fuel 是“每次调用”的预算。不补充的话，第一个大章节批次就烧光了，
    /// 之后每次调用都 trap——这正是吞掉整个下载的 bug。
    #[test]
    fn fuel_is_replenished_before_every_call() {
        let engine = shared_engine();
        let module = wasmtime::Module::new(
            engine,
            r#"(module (func (export "burn") (local i32)
                 (local.set 0 (i32.const 200000))
                 (loop $l
                   (local.set 0 (i32.sub (local.get 0) (i32.const 1)))
                   (br_if $l (local.get 0)))))"#,
        )
        .expect("编译耗算力模块");
        let storage = std::env::temp_dir().join(format!("ilp-fuel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&storage);
        let mut store =
            new_store(engine, HostState::new("fuel".to_string(), storage.clone())).unwrap();
        let instance = wasmtime::Instance::new(&mut store, &module, &[]).expect("实例化");
        let burn = instance
            .get_typed_func::<(), ()>(&mut store, "burn")
            .expect("取 burn");

        // 空转换器都不剩多少燃料，足以证明“下次调用还能继续跑”。
        for round in 0..3 {
            guest_call(&mut store, |s| burn.call(s, ()))
                .unwrap_or_else(|e| panic!("第 {round} 次调用不该缺燃料: {e}"));
            let left = store.get_fuel().expect("启用了 fuel");
            assert!(left > 0, "第 {round} 次调用后燃料应为正数，实际 {left}");
        }

        let _ = std::fs::remove_dir_all(&storage);
    }
}
