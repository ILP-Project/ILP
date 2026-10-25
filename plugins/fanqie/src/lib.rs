use ilp_types::{ChapterEntry, ParseInput, PluginMetadata, SearchResult};
use serde_json::Value;
use std::alloc::{alloc, dealloc, Layout};
use std::sync::Mutex;

const DETAIL_API: &str =
    "https://fanqienovel.com/reading/bookapi/multi-detail/v/?aid=1967&iid=1&version_code=999&book_id=";
const CONTENT_API: &str = "https://fanqienovel.com/api/reader/directory/detail?bookId=";
const CHAPTER_API: &str = "https://novel.snssdk.com/api/novel/reader/full/v1/?item_id=";

#[link(wasm_import_module = "ilp")]
extern "C" {
    fn ilp_http_get_size(url: *const u8, url_len: usize) -> i64;
    fn ilp_http_get(url: *const u8, url_len: usize, buf: *mut u8, buf_len: usize) -> i64;
    fn ilp_log(level: i32, msg: *const u8, msg_len: usize);
}

struct State {
    id: String,
    detail: Option<Value>,
    title: Option<String>,
    author: Option<String>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn with_state<T>(f: impl FnOnce(&mut State) -> T) -> T {
    let mut guard = STATE.lock().unwrap();
    let state = guard.get_or_insert_with(|| State {
        id: String::new(),
        detail: None,
        title: None,
        author: None,
    });
    f(state)
}

fn log_info(msg: &str) {
    let b = msg.as_bytes();
    unsafe { ilp_log(2, b.as_ptr(), b.len()) }
}

fn http_fetch(url: &str) -> Option<String> {
    let b = url.as_bytes();
    let size = unsafe { ilp_http_get_size(b.as_ptr(), b.len()) };
    if size <= 0 {
        return None;
    }
    let mut buf = vec![0u8; size as usize];
    let n = unsafe { ilp_http_get(b.as_ptr(), b.len(), buf.as_mut_ptr(), buf.len()) };
    if n <= 0 {
        return None;
    }
    buf.truncate(n as usize);
    Some(String::from_utf8_lossy(&buf).into_owned())
}

fn fetch_detail(force: bool) -> Option<Value> {
    if !force {
        if let Some(d) = with_state(|s| s.detail.clone()) {
            return Some(d);
        }
    }
    let id = with_state(|s| s.id.clone());
    let url = format!("{DETAIL_API}{id}");
    let text = http_fetch(&url)?;
    let v: Value = serde_json::from_str(&text).ok()?;
    with_state(|s| s.detail = Some(v.clone()));
    Some(v)
}

fn data_field<'a>(detail: &'a Value, key: &str) -> &'a str {
    detail
        .get("data")
        .and_then(|d| d.get(0))
        .and_then(|d| d.get(key))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
}

#[no_mangle]
pub extern "C" fn ilp_alloc(len: i32) -> i32 {
    if len <= 0 {
        return 0;
    }
    let layout = Layout::from_size_align(len as usize, 1).unwrap();
    unsafe { alloc(layout) as i32 }
}

#[no_mangle]
pub extern "C" fn ilp_free(ptr: i32, len: i32) {
    if ptr == 0 || len <= 0 {
        return;
    }
    let layout = Layout::from_size_align(len as usize, 1).unwrap();
    unsafe { dealloc(ptr as *mut u8, layout) }
}

fn pack(ptr: i32, len: i32) -> i64 {
    ((ptr as i64) << 32) | (len as i64 & 0xFFFF_FFFF)
}

fn read_string(ptr: i32, len: i32) -> String {
    if ptr == 0 || len <= 0 {
        return String::new();
    }
    let slice = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
    String::from_utf8_lossy(slice).into_owned()
}

fn return_string(s: &str) -> i64 {
    if s.is_empty() {
        return 0;
    }
    let len = s.len() as i32;
    let ptr = ilp_alloc(len);
    if ptr == 0 {
        return 0;
    }
    unsafe { std::ptr::copy_nonoverlapping(s.as_ptr(), ptr as *mut u8, len as usize) }
    pack(ptr, len)
}

fn return_json<T: serde::Serialize>(v: &T) -> i64 {
    match serde_json::to_string(v) {
        Ok(s) => return_string(&s),
        Err(_) => 0,
    }
}

#[no_mangle]
pub extern "C" fn ilp_get_metadata() -> i64 {
    let meta = PluginMetadata {
        site_name: "番茄小说".to_string(),
        site_id: "fanqie".to_string(),
        site_domain: "fanqienovel.com".to_string(),
        plugin_name: "FanqiePlugin".to_string(),
        plugin_id: "fanqieplugin".to_string(),
        version: "1.0.0".to_string(),
    };
    return_json(&meta)
}

#[no_mangle]
pub extern "C" fn ilp_init(id_ptr: i32, id_len: i32, force: i32) -> i32 {
    let id = read_string(id_ptr, id_len);
    with_state(|s| {
        s.id = id;
        if force != 0 {
            s.detail = None;
            s.title = None;
            s.author = None;
        }
    });
    0
}

#[no_mangle]
pub extern "C" fn ilp_get_title() -> i64 {
    if let Some(t) = with_state(|s| s.title.clone()) {
        return return_string(&t);
    }
    let title = fetch_detail(false)
        .map(|d| data_field(&d, "book_name").to_string())
        .unwrap_or_default();
    with_state(|s| s.title = Some(title.clone()));
    return_string(&title)
}

#[no_mangle]
pub extern "C" fn ilp_get_author() -> i64 {
    if let Some(a) = with_state(|s| s.author.clone()) {
        return return_string(&a);
    }
    let author = fetch_detail(false)
        .map(|d| data_field(&d, "author").to_string())
        .unwrap_or_default();
    with_state(|s| s.author = Some(author.clone()));
    return_string(&author)
}

#[no_mangle]
pub extern "C" fn ilp_get_cover() -> i64 {
    let cover = fetch_detail(false)
        .map(|d| data_field(&d, "expand_thumb_url").to_string())
        .unwrap_or_default();
    return_string(&cover)
}

#[no_mangle]
pub extern "C" fn ilp_get_content(_force: i32) -> i64 {
    let id = with_state(|s| s.id.clone());
    let url = format!("{CONTENT_API}{id}");
    let Some(text) = http_fetch(&url) else {
        log_info("获取目录失败");
        return 0;
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        log_info("解析目录失败");
        return 0;
    };

    let update_time: i64 = fetch_detail(false)
        .map(|d| {
            data_field(&d, "last_chapter_update_time")
                .parse()
                .unwrap_or(0)
        })
        .unwrap_or(0);

    let mut chapters = Vec::new();
    if let Some(volumes) = v
        .get("data")
        .and_then(|d| d.get("chapterListWithVolume"))
        .and_then(|v| v.as_array())
    {
        for volume in volumes {
            if let Some(list) = volume.as_array() {
                for ch in list {
                    let item_id = ch
                        .get("itemId")
                        .and_then(|x| x.as_str())
                        .unwrap_or_default();
                    let title = ch.get("title").and_then(|x| x.as_str()).unwrap_or_default();
                    chapters.push(ChapterEntry {
                        id: item_id.to_string(),
                        title: title.to_string(),
                        url: format!("https://fanqienovel.com/reader/{item_id}"),
                        fetch_url: format!("{CHAPTER_API}{item_id}"),
                        update_time,
                    });
                }
            }
        }
    }
    return_json(&chapters)
}

#[no_mangle]
pub extern "C" fn ilp_search(_kw_ptr: i32, _kw_len: i32) -> i64 {
    let empty: Vec<SearchResult> = Vec::new();
    return_json(&empty)
}

#[no_mangle]
pub extern "C" fn ilp_parse_chapter(data_ptr: i32, data_len: i32) -> i64 {
    let s = read_string(data_ptr, data_len);
    let Ok(input) = serde_json::from_str::<ParseInput>(&s) else {
        return 0;
    };
    let Ok(v) = serde_json::from_str::<Value>(&input.content) else {
        return 0;
    };
    let html = v
        .get("data")
        .and_then(|d| d.get("content"))
        .and_then(|x| x.as_str())
        .unwrap_or_default();
    return_string(&strip_html(html))
}

fn strip_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_tag = false;
    for c in input.chars() {
        match c {
            '<' => {
                in_tag = true;
                out.push('\n');
            }
            '>' => {
                in_tag = false;
            }
            _ if in_tag => {}
            _ => out.push(c),
        }
    }
    out
}
