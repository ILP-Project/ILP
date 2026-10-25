use serde_json::Value;
use std::cell::RefCell;
use std::collections::HashMap;

wit_bindgen::generate!({
    world: "novel-plugin",
    path: "../../wit",
});

use exports::ilp::plugin::plugin::{
    ChapterContent, ChapterEntry, FetchTask, Guest, PluginMetadata, SearchPage,
};
use ilp::plugin::host;
use ilp::plugin::types::{HttpRequest, HttpResponse};

const CATALOG_API: &str = "https://fanqienovel.com/api/reader/directory/detail?bookId=";
const PAGE_API: &str = "https://fanqienovel.com/page/";
const READER_API: &str = "https://fanqienovel.com/reader/";

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

struct State {
    book_id: String,
    page_html: Option<String>,
    title: Option<String>,
    author: Option<String>,
    cover_url: Option<String>,
    description: Option<String>,
    /// 目录里拿到 id -> 标题，consume_fetch 用它补全章节名。
    /// 任务里只带 id，标题必须回查，否则所有章节会写到同一个文件。
    titles: HashMap<String, String>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn with_state<T>(f: impl FnOnce(&mut State) -> T) -> T {
    STATE.with(|cell| {
        let mut guard = cell.borrow_mut();
        let state = guard.get_or_insert_with(|| State {
            book_id: String::new(),
            page_html: None,
            title: None,
            author: None,
            cover_url: None,
            description: None,
            titles: HashMap::new(),
        });
        f(state)
    })
}

fn log_info(msg: &str) {
    host::log(2, msg);
}

fn log_warn(msg: &str) {
    host::log(1, msg);
}

fn get(url: &str) -> Result<String, String> {
    let resp = host::http_fetch(&HttpRequest {
        method: "GET".to_string(),
        url: url.to_string(),
        headers: vec![ilp::plugin::types::HttpHeader {
            name: "user-agent".to_string(),
            value: UA.to_string(),
        }],
        body: None,
    })?;
    if resp.status != 200 {
        return Err(format!("HTTP {}: {url}", resp.status));
    }
    String::from_utf8(resp.body).map_err(|e| format!("响应不是合法 UTF-8: {e}"))
}

/// 书籍页 HTML。标题/作者/封面/简介全在这里，一次请求拿到四样，
/// 避免原版 getTitle/getCover/getContent 各拉一次 detail API。
fn ensure_page(force: bool) -> Result<String, String> {
    if !force {
        if let Some(html) = with_state(|s| s.page_html.clone()) {
            return Ok(html);
        }
    }
    let book_id = with_state(|s| s.book_id.clone());
    if book_id.is_empty() {
        return Err("尚未指定书籍 id".to_string());
    }
    let html = get(&format!("{PAGE_API}{book_id}"))?;
    with_state(|s| {
        s.page_html = Some(html.clone());
        s.title = None;
        s.author = None;
        s.cover_url = None;
        s.description = None;
    });
    Ok(html)
}

/// 页面里的 JSON-LD 是结构化元数据的权威来源：
/// 标题（h1）、作者名、封面地址、简介都在这里，
/// 比逐标签猜要稳。
fn json_ld_blocks(html: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let mut from = 0usize;
    while let Some(pos) = html[from..].find("ld+json") {
        let after = from + pos;
        let Some(gt) = html[after..].find('>') else {
            break;
        };
        let body_start = after + gt + 1;
        let Some(end) = html[body_start..].find("</script>") else {
            break;
        };
        if let Ok(v) = serde_json::from_str::<Value>(&html[body_start..body_start + end]) {
            out.push(v);
        }
        from = body_start + end;
    }
    out
}

fn ld_str(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(str::to_string)
}

fn extract_title(html: &str) -> String {
    first_tag_text(html, "h1").unwrap_or_default()
}

fn extract_author(html: &str) -> String {
    for v in json_ld_blocks(html) {
        if let Some(a) = v
            .get("author")
            .and_then(|a| a.as_array())
            .and_then(|a| a.first())
            .and_then(|a| a.get("name"))
            .and_then(|n| n.as_str())
        {
            return a.to_string();
        }
    }
    String::new()
}

fn extract_cover(html: &str) -> String {
    for v in json_ld_blocks(html) {
        for key in ["image", "images"] {
            if let Some(url) = v
                .get(key)
                .and_then(|i| i.as_array())
                .and_then(|i| i.first())
                .and_then(|i| i.as_str())
            {
                return url.to_string();
            }
        }
    }
    String::new()
}

fn extract_description(html: &str) -> String {
    for v in json_ld_blocks(html) {
        if let Some(d) = ld_str(&v, "description") {
            return d;
        }
    }
    attr_content(html, "meta", "name", "description").unwrap_or_default()
}

fn first_tag_text(html: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let start = html.find(&open)?;
    let after = html[start..].find('>')? + start;
    let end = html[after..].find(&format!("</{tag}>"))? + after;
    Some(html[after + 1..end].trim().to_string())
}

fn attr_content(html: &str, tag: &str, attr: &str, value: &str) -> Option<String> {
    let needle = format!("{attr}=\"{value}\"");
    let open = format!("<{tag}");
    let mut from = 0usize;
    while let Some(pos) = html[from..].find(&open) {
        let abs = from + pos;
        let close = html[abs..].find('>')? + abs;
        let head = &html[abs..close];
        if head.contains(&needle) {
            let cpos = head.find("content=\"")?;
            let rest = &head[cpos + "content=\"".len()..];
            let end = rest.find('"')?;
            return Some(rest[..end].to_string());
        }
        from = close;
    }
    None
}

struct Fanqie;

impl Guest for Fanqie {
    fn metadata() -> PluginMetadata {
        PluginMetadata {
            site_name: "番茄小说".to_string(),
            site_id: "fanqie".to_string(),
            site_domain: "fanqienovel.com".to_string(),
            plugin_name: "FanqiePlugin".to_string(),
            plugin_id: "fanqieplugin".to_string(),
            version: "1.0.0".to_string(),
        }
    }

    fn init(book_id: String, force_update: bool) -> Result<(), String> {
        with_state(|s| {
            s.book_id = book_id;
            if force_update {
                s.page_html = None;
                s.title = None;
                s.author = None;
                s.cover_url = None;
                s.description = None;
            }
        });
        Ok(())
    }

    fn title() -> Result<String, String> {
        if let Some(t) = with_state(|s| s.title.clone()) {
            return Ok(t);
        }
        let html = ensure_page(false)?;
        let title = extract_title(&html);
        with_state(|s| s.title = Some(title.clone()));
        Ok(title)
    }

    fn author() -> Result<String, String> {
        if let Some(a) = with_state(|s| s.author.clone()) {
            return Ok(a);
        }
        let html = ensure_page(false)?;
        let author = extract_author(&html);
        with_state(|s| s.author = Some(author.clone()));
        Ok(author)
    }

    fn cover_url() -> Result<String, String> {
        if let Some(c) = with_state(|s| s.cover_url.clone()) {
            return Ok(c);
        }
        let html = ensure_page(false)?;
        let cover = extract_cover(&html);
        with_state(|s| s.cover_url = Some(cover.clone()));
        Ok(cover)
    }

    fn description() -> Result<String, String> {
        if let Some(d) = with_state(|s| s.description.clone()) {
            return Ok(d);
        }
        let html = ensure_page(false)?;
        let desc = extract_description(&html);
        with_state(|s| s.description = Some(desc.clone()));
        Ok(desc)
    }

    fn catalog(force_update: bool) -> Result<Vec<ChapterEntry>, String> {
        let book_id = with_state(|s| s.book_id.clone());
        if book_id.is_empty() {
            return Err("尚未指定书籍 id".to_string());
        }
        let text = get(&format!("{CATALOG_API}{book_id}"))?;
        let v: Value =
            serde_json::from_str(&text).map_err(|e| format!("目录 JSON 解析失败: {e}"))?;

        let mut out = Vec::new();
        let volumes = v
            .get("data")
            .and_then(|d| d.get("chapterListWithVolume"))
            .and_then(|v| v.as_array());
        let Some(volumes) = volumes else {
            log_warn("目录结构里没有 chapterListWithVolume");
            return Ok(out);
        };
        for volume in volumes {
            let Some(list) = volume.as_array() else {
                continue;
            };
            for ch in list {
                let id = ch
                    .get("itemId")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default();
                if id.is_empty() {
                    continue;
                }
                let title = ch.get("title").and_then(|x| x.as_str()).unwrap_or_default();
                out.push(ChapterEntry {
                    id: id.to_string(),
                    title: title.to_string(),
                    url: format!("{READER_API}{id}"),
                    fetch_url: format!("{READER_API}{id}"),
                    update_time: 0,
                });
            }
        }
        let _ = force_update;
        with_state(|s| {
            s.titles = out
                .iter()
                .map(|c| (c.id.clone(), c.title.clone()))
                .collect();
        });
        log_info(&format!("目录共 {} 章", out.len()));
        Ok(out)
    }

    fn search(keyword: String, offset: u32) -> Result<SearchPage, String> {
        let _ = (keyword, offset);
        Ok(SearchPage {
            results: vec![],
            offset: 0,
            has_more: false,
        })
    }

    fn plan_fetch(chapter_ids: Vec<String>) -> Result<Vec<FetchTask>, String> {
        Ok(chapter_ids
            .into_iter()
            .map(|id| FetchTask {
                id: id.clone(),
                request: HttpRequest {
                    method: "GET".to_string(),
                    url: format!("{READER_API}{id}"),
                    headers: vec![ilp::plugin::types::HttpHeader {
                        name: "user-agent".to_string(),
                        value: UA.to_string(),
                    }],
                    body: None,
                },
            })
            .collect())
    }

    fn consume_fetch(
        task_id: String,
        response: HttpResponse,
    ) -> Result<Vec<ChapterContent>, String> {
        if response.status != 200 {
            return Err(format!("HTTP {}: {task_id}", response.status));
        }
        let html =
            String::from_utf8(response.body).map_err(|e| format!("响应不是合法 UTF-8: {e}"))?;
        let text = strip_html(&html);
        if text.trim().is_empty() {
            return Ok(vec![]);
        }
        let title = with_state(|s| s.titles.get(&task_id).cloned())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| task_id.clone());
        Ok(vec![ChapterContent {
            id: task_id,
            title,
            text,
        }])
    }
}

/// 旧版用正则会话剥标签，脆弱且会把脚本样式一起带进来。
/// 这里只取标签外的文本并压缩空白，真正的 DOM 抽取留给后续提交。
fn strip_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_tag = false;
    for c in input.chars() {
        match c {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push('\n');
            }
            _ if in_tag => {}
            _ => out.push(c),
        }
    }
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

export!(Fanqie);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_from_h1() {
        let html = "<html><h1>书名</h1></html>";
        assert_eq!(extract_title(html), "书名");
    }

    #[test]
    fn cover_from_og_image() {
        let html = r#"<meta property="og:image" content="https://x/c.jpg">"#;
        assert_eq!(extract_cover(html), "https://x/c.jpg");
    }

    #[test]
    fn strip_html_keeps_text_only() {
        assert_eq!(strip_html("<p>a</p><p>b</p>"), "a\nb");
    }
}
