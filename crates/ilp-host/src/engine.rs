use anyhow::{anyhow, Context, Result};
use ilp_types::{ChapterEntry, PluginMetadata, SearchResult};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use wasmtime::{Caller, Engine, Instance, Linker, Memory, Module, Store, TypedFunc};
use wasmtime_wasi::p1::{self, WasiP1Ctx};
use wasmtime_wasi::WasiCtxBuilder;

use crate::http;

type HttpCache = Arc<Mutex<HashMap<String, Vec<u8>>>>;

static HTTP_CACHE: OnceLock<HttpCache> = OnceLock::new();

fn http_cache() -> &'static HttpCache {
    HTTP_CACHE.get_or_init(|| Arc::new(Mutex::new(HashMap::new())))
}

pub struct PluginInstance {
    pub site_id: String,
    store: Store<WasiP1Ctx>,
    instance: Instance,
}

fn unpack(packed: i64) -> (i32, i32) {
    ((packed >> 32) as i32, (packed & 0xFFFF_FFFF) as i32)
}

impl PluginInstance {
    pub fn load(path: &Path) -> Result<Self> {
        let engine = Engine::default();
        let module = Module::from_file(&engine, path)?;
        let mut linker: Linker<WasiP1Ctx> = Linker::new(&engine);
        p1::add_to_linker_sync(&mut linker, |cx: &mut WasiP1Ctx| cx)?;
        define_host_funcs(&mut linker)?;
        let wasi_ctx = WasiCtxBuilder::new().inherit_stdio().build_p1();
        let mut store = Store::new(&engine, wasi_ctx);
        let instance = linker.instantiate(&mut store, &module)?;

        let metadata = Self::get_metadata(&mut store, &instance)?;
        let site_id = metadata.site_id.clone();

        Ok(Self {
            site_id,
            store,
            instance,
        })
    }

    fn get_metadata(store: &mut Store<WasiP1Ctx>, instance: &Instance) -> Result<PluginMetadata> {
        let f: TypedFunc<(), i64> = instance.get_typed_func(&mut *store, "ilp_get_metadata")?;
        let packed = f.call(&mut *store, ())?;
        let s = read_packed_string(store, instance, packed)?;
        serde_json::from_str(&s).context("解析插件元数据失败")
    }

    pub fn init(&mut self, id: &str, force_update: bool) -> Result<()> {
        let (ptr, len) = write_guest(&mut self.store, &self.instance, id.as_bytes())?;
        let f: TypedFunc<(i32, i32, i32), i32> =
            self.instance.get_typed_func(&mut self.store, "ilp_init")?;
        f.call(&mut self.store, (ptr, len, force_update as i32))?;
        if ptr != 0 {
            free_guest(&mut self.store, &self.instance, ptr, len);
        }
        Ok(())
    }

    fn call_string(&mut self, name: &str) -> Result<String> {
        let f: TypedFunc<(), i64> = self.instance.get_typed_func(&mut self.store, name)?;
        let packed = f.call(&mut self.store, ())?;
        read_packed_string(&mut self.store, &self.instance, packed)
    }

    pub fn get_title(&mut self) -> Result<String> {
        self.call_string("ilp_get_title")
    }

    pub fn get_author(&mut self) -> Result<String> {
        self.call_string("ilp_get_author")
    }

    pub fn get_cover(&mut self) -> Result<String> {
        self.call_string("ilp_get_cover")
    }

    pub fn get_content(&mut self, force: bool) -> Result<Vec<ChapterEntry>> {
        let f: TypedFunc<i32, i64> = self
            .instance
            .get_typed_func(&mut self.store, "ilp_get_content")?;
        let packed = f.call(&mut self.store, force as i32)?;
        let s = read_packed_string(&mut self.store, &self.instance, packed)?;
        serde_json::from_str(&s).context("解析章节列表失败")
    }

    pub fn search(&mut self, keyword: &str) -> Result<Vec<SearchResult>> {
        let (ptr, len) = write_guest(&mut self.store, &self.instance, keyword.as_bytes())?;
        let f: TypedFunc<(i32, i32), i64> = self
            .instance
            .get_typed_func(&mut self.store, "ilp_search")?;
        let packed = f.call(&mut self.store, (ptr, len))?;
        if ptr != 0 {
            free_guest(&mut self.store, &self.instance, ptr, len);
        }
        let s = read_packed_string(&mut self.store, &self.instance, packed)?;
        serde_json::from_str(&s).context("解析搜索结果失败")
    }

    pub fn parse_chapter(&mut self, title: &str, content: &str) -> Result<String> {
        let input = ilp_types::ParseInput {
            title: title.to_string(),
            content: content.to_string(),
        };
        let json = serde_json::to_string(&input)?;
        let (ptr, len) = write_guest(&mut self.store, &self.instance, json.as_bytes())?;
        let f: TypedFunc<(i32, i32), i64> = self
            .instance
            .get_typed_func(&mut self.store, "ilp_parse_chapter")?;
        let packed = f.call(&mut self.store, (ptr, len))?;
        if ptr != 0 {
            free_guest(&mut self.store, &self.instance, ptr, len);
        }
        read_packed_string(&mut self.store, &self.instance, packed)
    }
}

pub fn load_plugins() -> Result<Vec<PluginInstance>> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir("plugins") {
        Ok(e) => e,
        Err(_) => return Ok(out),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("wasm") {
            match PluginInstance::load(&path) {
                Ok(p) => out.push(p),
                Err(e) => tracing::warn!("加载插件 {} 失败: {:#}", path.display(), e),
            }
        }
    }
    Ok(out)
}

fn read_packed_string(
    store: &mut Store<WasiP1Ctx>,
    instance: &Instance,
    packed: i64,
) -> Result<String> {
    if packed <= 0 {
        return Ok(String::new());
    }
    let (ptr, len) = unpack(packed);
    let mem = instance
        .get_memory(&mut *store, "memory")
        .ok_or_else(|| anyhow!("插件未导出 memory"))?;
    let mut buf = vec![0u8; len as usize];
    mem.read(&mut *store, ptr as usize, &mut buf)?;
    free_guest(store, instance, ptr, len);
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn free_guest(store: &mut Store<WasiP1Ctx>, instance: &Instance, ptr: i32, len: i32) {
    if ptr == 0 || len <= 0 {
        return;
    }
    if let Ok(f) = instance.get_typed_func::<(i32, i32), ()>(&mut *store, "ilp_free") {
        let _ = f.call(&mut *store, (ptr, len));
    }
}

fn write_guest(
    store: &mut Store<WasiP1Ctx>,
    instance: &Instance,
    data: &[u8],
) -> Result<(i32, i32)> {
    if data.is_empty() {
        return Ok((0, 0));
    }
    let alloc: TypedFunc<i32, i32> = instance.get_typed_func(&mut *store, "ilp_alloc")?;
    let ptr = alloc.call(&mut *store, data.len() as i32)?;
    let mem = instance
        .get_memory(&mut *store, "memory")
        .ok_or_else(|| anyhow!("插件未导出 memory"))?;
    mem.write(&mut *store, ptr as usize, data)?;
    Ok((ptr, data.len() as i32))
}

fn read_caller_str(
    mem: &Memory,
    caller: &mut Caller<'_, WasiP1Ctx>,
    ptr: i32,
    len: i32,
) -> Option<String> {
    if len <= 0 || ptr < 0 {
        return Some(String::new());
    }
    let mut buf = vec![0u8; len as usize];
    mem.read(&mut *caller, ptr as usize, &mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

fn define_host_funcs(linker: &mut Linker<WasiP1Ctx>) -> Result<()> {
    linker.func_wrap(
        "ilp",
        "ilp_http_get_size",
        |mut caller: Caller<'_, WasiP1Ctx>, url_ptr: i32, url_len: i32| -> i64 {
            let Some(mem) = caller.get_export("memory").and_then(|e| e.into_memory()) else {
                return -1;
            };
            let Some(url) = read_caller_str(&mem, &mut caller, url_ptr, url_len) else {
                return -1;
            };
            match http::get_bytes(&url) {
                Ok(bytes) => {
                    let len = bytes.len() as i64;
                    http_cache().lock().unwrap().insert(url, bytes);
                    len
                }
                Err(_) => -1,
            }
        },
    )?;

    linker.func_wrap(
        "ilp",
        "ilp_http_get",
        |mut caller: Caller<'_, WasiP1Ctx>,
         url_ptr: i32,
         url_len: i32,
         buf_ptr: i32,
         buf_len: i32|
         -> i64 {
            let Some(mem) = caller.get_export("memory").and_then(|e| e.into_memory()) else {
                return -1;
            };
            let Some(url) = read_caller_str(&mem, &mut caller, url_ptr, url_len) else {
                return -1;
            };
            let cache = http_cache().lock().unwrap();
            let Some(bytes) = cache.get(&url) else {
                return -1;
            };
            let n = bytes.len().min(buf_len as usize);
            if mem
                .write(&mut caller, buf_ptr as usize, &bytes[..n])
                .is_err()
            {
                return -1;
            }
            n as i64
        },
    )?;

    linker.func_wrap(
        "ilp",
        "ilp_log",
        |mut caller: Caller<'_, WasiP1Ctx>, level: i32, ptr: i32, len: i32| {
            let Some(mem) = caller.get_export("memory").and_then(|e| e.into_memory()) else {
                return;
            };
            let msg = read_caller_str(&mem, &mut caller, ptr, len).unwrap_or_default();
            match level {
                0 => tracing::error!("{}", msg),
                1 => tracing::warn!("{}", msg),
                2 => tracing::info!("{}", msg),
                _ => tracing::debug!("{}", msg),
            }
        },
    )?;

    Ok(())
}
