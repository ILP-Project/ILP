use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use wasmtime::component::{Component, Linker};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{ResourceTable, WasiCtx, WasiCtxView, WasiView};

wasmtime::component::bindgen!({
    world: "novel-plugin",
    path: "../ilp-plugin-wit/wit",
});

use ilp::plugin::host::Host as HostTrait;
use ilp::plugin::types::{FetchTask, HttpResponse};

pub use ilp::plugin::types as wit_types;

use ilp_types::{ChapterContent, ChapterEntry, PluginMetadata, SearchResult};

/// 宿主给插件的全部能力对应的状态。
pub struct HostState {
    ctx: WasiCtx,
    table: ResourceTable,
    /// 插件名，用于隔离 storage 命名空间
    plugin_name: String,
    storage_dir: PathBuf,
    /// 内存里的 KV，落盘前先命中这里
    cache: HashMap<String, Vec<u8>>,
}

impl HostState {
    fn new(plugin_name: String, storage_dir: PathBuf) -> Self {
        Self {
            ctx: WasiCtx::builder().build(),
            table: ResourceTable::new(),
            plugin_name,
            storage_dir,
            cache: HashMap::new(),
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
        request: ilp::plugin::host::HttpRequest,
    ) -> Result<ilp::plugin::host::HttpResponse, String> {
        let req = ilp::plugin::types::HttpRequest {
            method: request.method,
            url: request.url,
            headers: request.headers,
            body: request.body,
        };
        let resp = crate::http::send(&req).map_err(|e| format!("{e:#}"))?;
        Ok(resp)
    }

    fn storage_get(&mut self, key: String) -> Option<Vec<u8>> {
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
        if !is_safe_key(&key) {
            return Err(format!("非法 storage key: {key}"));
        }
        std::fs::create_dir_all(&self.storage_dir).map_err(|e| e.to_string())?;
        atomic_write(&self.storage_dir.join(&key), &value).map_err(|e| e.to_string())?;
        self.cache.insert(key, value);
        Ok(())
    }

    fn storage_delete(&mut self, key: String) -> Result<(), String> {
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

fn is_safe_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 128
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !key.starts_with('.')
        && !key.contains("..")
}

fn atomic_write(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)
}

/// 已加载的插件。
pub struct PluginInstance {
    pub metadata: PluginMetadata,
    store: Store<HostState>,
    bindings: NovelPlugin,
}

impl PluginInstance {
    pub fn load(path: &Path, storage_root: &Path) -> Result<Self> {
        let engine = Engine::default();
        let component = Component::from_file(&engine, path)
            .map_err(|e| anyhow!("读取组件失败 {}: {e}", path.display()))?;

        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("plugin")
            .to_string();

        let mut linker: Linker<HostState> = Linker::new(&engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
            .map_err(|e| anyhow!("注册 WASI p2 失败: {e}"))?;
        NovelPlugin::add_to_linker::<HostState, wasmtime::component::HasSelf<HostState>>(
            &mut linker,
            |s: &mut HostState| s,
        )?;

        let state = HostState::new(name.clone(), storage_root.join(&name));
        let mut store = Store::new(&engine, state);
        let bindings = NovelPlugin::instantiate(&mut store, &component, &linker)
            .map_err(|e| anyhow!("实例化插件失败 {}: {e}", path.display()))?;

        let raw = bindings
            .ilp_plugin_plugin()
            .call_metadata(&mut store)
            .map_err(|e| anyhow!("调用插件 metadata 失败: {e}"))?;
        let metadata = PluginMetadata {
            abi_version: raw.abi_version.clone(),
            site_name: raw.site_name,
            site_id: raw.site_id,
            site_domain: raw.site_domain,
            plugin_name: raw.plugin_name,
            plugin_id: raw.plugin_id,
            version: raw.version,
        };

        // ABI 不匹配必须在这里挡住。否则插件能"加载成功"，
        // 然后在某个具体调用的参数编解码里炸出一个与真实原因无关的错误。
        if metadata.abi_version != crate::plugin_manager::SUPPORTED_ABI {
            return Err(anyhow!(
                "插件 {} 的 ABI 版本为 {}，本版本 ilp 支持 {}。请升级插件或 ilp",
                metadata.plugin_name,
                metadata.abi_version,
                crate::plugin_manager::SUPPORTED_ABI
            ));
        }

        Ok(Self {
            metadata,
            store,
            bindings,
        })
    }

    pub fn site_id(&self) -> &str {
        &self.metadata.site_id
    }

    pub fn init(&mut self, book_id: &str, force_update: bool) -> Result<()> {
        let g = self.bindings.ilp_plugin_plugin();
        g.call_init(&mut self.store, book_id, force_update)
            .map_err(|e| anyhow!("plugin::{e}"))?
            .map_err(|e| anyhow!("plugin::{e}"))
    }

    pub fn title(&mut self) -> Result<String> {
        let g = self.bindings.ilp_plugin_plugin();
        g.call_title(&mut self.store)
            .map_err(|e| anyhow!("plugin::{e}"))?
            .map_err(|e| anyhow!("plugin::{e}"))
    }

    pub fn author(&mut self) -> Result<String> {
        let g = self.bindings.ilp_plugin_plugin();
        g.call_author(&mut self.store)
            .map_err(|e| anyhow!("plugin::{e}"))?
            .map_err(|e| anyhow!("plugin::{e}"))
    }

    pub fn cover_url(&mut self) -> Result<String> {
        let g = self.bindings.ilp_plugin_plugin();
        g.call_cover_url(&mut self.store)
            .map_err(|e| anyhow!("plugin::{e}"))?
            .map_err(|e| anyhow!("plugin::{e}"))
    }

    pub fn description(&mut self) -> Result<String> {
        let g = self.bindings.ilp_plugin_plugin();
        g.call_description(&mut self.store)
            .map_err(|e| anyhow!("plugin::{e}"))?
            .map_err(|e| anyhow!("plugin::{e}"))
    }

    pub fn catalog(&mut self, force: bool) -> Result<Vec<ChapterEntry>> {
        let g = self.bindings.ilp_plugin_plugin();
        let raw = g
            .call_catalog(&mut self.store, force)
            .map_err(|e| anyhow!("plugin::{e}"))?
            .map_err(|e| anyhow!("plugin::{e}"))?;
        Ok(raw.into_iter().map(convert_entry).collect())
    }

    /// `cursor` 为空表示取第一页，否则原样传上一页返回的游标。
    pub fn search(&mut self, keyword: &str, cursor: &str) -> Result<ilp_types::SearchPage> {
        let g = self.bindings.ilp_plugin_plugin();
        let raw = g
            .call_search(&mut self.store, keyword, cursor)
            .map_err(|e| anyhow!("plugin::{e}"))?
            .map_err(|e| anyhow!("plugin::{e}"))?;
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

    pub fn plan_fetch(&mut self, ids: &[String]) -> Result<Vec<FetchTask>> {
        let g = self.bindings.ilp_plugin_plugin();
        g.call_plan_fetch(&mut self.store, ids)
            .map_err(|e| anyhow!("plugin::{e}"))?
            .map_err(|e| anyhow!("plugin::{e}"))
    }

    pub fn consume_fetch(
        &mut self,
        task_id: &str,
        resp: HttpResponse,
    ) -> Result<Vec<ChapterContent>> {
        let g = self.bindings.ilp_plugin_plugin();
        let raw = g
            .call_consume_fetch(&mut self.store, task_id, &resp)
            .map_err(|e| anyhow!("plugin::{e}"))?
            .map_err(|e| anyhow!("plugin::{e}"))?;
        Ok(raw
            .into_iter()
            .map(|c| ChapterContent {
                id: c.id,
                title: c.title,
                text: c.text,
            })
            .collect())
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

    for path in paths {
        match PluginInstance::load(&path, &storage_root) {
            Ok(p) => out.push(p),
            Err(e) => tracing::warn!("加载插件 {} 失败: {e:#}", path.display()),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

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
