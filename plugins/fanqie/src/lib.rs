//! 番茄小说插件（WASM guest）。
//!
//! 正文走 APP 网关（网页阅读页已被风控封死），目录与元数据走网页接口。
#![recursion_limit = "256"]

mod clock;
mod gateway;
mod sign;
mod sm3;
mod spn;

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
use ilp::plugin::types::{HttpRequest, HttpResponse, SearchResult};

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

    fn search(keyword: String, cursor: String) -> Result<SearchPage, String> {
        let dev = gateway::ensure_device(false)?;
        let cur = gateway::SearchCursor::decode(&cursor);
        let (items, has_more, next) =
            gateway::search(&dev, &keyword, &cur, gateway::SEARCH_PAGE_SIZE)?;
        Ok(SearchPage {
            results: items
                .into_iter()
                .map(|i| SearchResult {
                    id: i.id,
                    title: i.title,
                    author: i.author,
                    cover_url: i.cover_url,
                    description: i.description,
                })
                .collect(),
            cursor: next.encode(),
            has_more,
        })
    }

    fn plan_fetch(chapter_ids: Vec<String>) -> Result<Vec<FetchTask>, String> {
        if chapter_ids.is_empty() {
            return Ok(Vec::new());
        }
        let dev = gateway::ensure_device(false)?;
        let book_id = with_state(|s| s.book_id.clone());
        if book_id.is_empty() {
            return Err("尚未抓取目录，不知道 book_id".to_string());
        }
        // 宿主可能直接从缓存加载书籍而没调 catalog，
        // 那样状态里没有标题映射。就地补一次，保证章节名正确。
        if with_state(|s| s.titles.is_empty()) {
            let _ = Self::catalog(false);
        }
        let (url, headers) = gateway::plan_batch(&dev, &book_id, &chapter_ids)?;
        Ok(vec![FetchTask {
            // 任务 id 带回这一批的章节 id，consume_fetch 才知道怎么拆。
            id: chapter_ids.join(","),
            request: HttpRequest {
                method: "GET".to_string(),
                url,
                headers,
                body: None,
            },
        }])
    }

    /// 拆一批网关响应。返回的章节数可能少于请求数
    /// （服务端限流时会少给），宿主据此降级为逐章重试。
    fn consume_fetch(
        task_id: String,
        response: HttpResponse,
    ) -> Result<Vec<ChapterContent>, String> {
        if response.status != 200 {
            return Err(format!("HTTP {}: {task_id}", response.status));
        }
        let dev = gateway::ensure_device(false)?;
        let wanted: Vec<String> = task_id.split(',').map(str::to_string).collect();
        let bodies = gateway::parse_batch(&dev, &response.body)?;

        let mut out = Vec::with_capacity(bodies.len());
        let mut missing = Vec::new();
        for id in &wanted {
            let Some(text) = bodies.get(id) else {
                // 服务端限速时会少返回若干章。必须记下来让宿主重试，
                // 否则这些章会被静默跳过、计入“成功”。
                missing.push(id.clone());
                continue;
            };
            if text.trim().is_empty() {
                missing.push(id.clone());
                continue;
            }
            let title = with_state(|s| s.titles.get(id).cloned())
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| id.clone());
            // APP 通道给的是 <blk> 结构化 HTML，必须渲染成纯文本再落盘。
            let body = strip_html(text);
            if body.trim().is_empty() {
                missing.push(id.clone());
                continue;
            }
            out.push(ChapterContent {
                id: id.clone(),
                title,
                text: body,
            });
        }
        if !missing.is_empty() {
            log_warn(&format!(
                "本批 {} 章里有 {} 章未返回，交给宿主重试",
                wanted.len(),
                missing.len()
            ));
        }
        Ok(out)
    }
}

/// 旧版用正则会话剥标签，脆弱且会把脚本样式一起带进来。
/// 这里只取标签外的文本并压缩空白，真正的 DOM 抽取留给后续提交。
/// 从章节 HTML 里抽出正文段落。
///
/// APP 通道返回的是 `<article><p><blk>…</blk></p>…</article>` 结构，
/// `<blk>` 才是正文文本节点；标题在 `<h1><blk>` 里。
/// 每个非空 `<blk>` 成为一段，空段落丢弃——与网页通道口径一致，
/// 两条通道产出的 .txt 应当字节相同。
fn strip_html(input: &str) -> String {
    let mut paragraphs = Vec::new();
    let mut rest = input;

    while let Some(start) = rest.find("<blk") {
        let Some(close) = rest[start..].find('>') else {
            break;
        };
        let body_start = start + close + 1;
        let Some(end) = rest[body_start..].find("</blk>") else {
            break;
        };
        let raw = &rest[body_start..body_start + end];
        let text = unescape_entities(raw);
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            paragraphs.push(trimmed.to_string());
        }
        rest = &rest[body_start + end + 6..];
    }

    if paragraphs.is_empty() {
        // 不是 blk 结构（网页通道的旧页面）就退回逐标签剥离。
        return strip_tags_fallback(input);
    }
    // 第一个 <blk> 在 <h1> 里，是章节标题，正文段落从第二个开始。
    if paragraphs.len() > 1 {
        paragraphs.remove(0);
    }
    paragraphs.join("\n")
}

fn strip_tags_fallback(input: &str) -> String {
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

fn unescape_entities(s: &str) -> String {
    s.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
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
    fn cover_from_json_ld() {
        let html = r#"<script nonce="x" type="application/ld+json">{"image":["https://x/c.jpg"]}</script>"#;
        assert_eq!(extract_cover(html), "https://x/c.jpg");
    }

    #[test]
    fn author_from_json_ld() {
        let html = r#"<script type="application/ld+json">{"author":[{"name":"丧狐"}]}</script>"#;
        assert_eq!(extract_author(html), "丧狐");
    }

    #[test]
    fn json_ld_skips_broken_blocks() {
        let html = r#"<script type="application/ld+json">not json</script>
            <script type="application/ld+json">{"description":"好故事"}</script>"#;
        assert_eq!(extract_description(html), "好故事");
    }

    #[test]
    fn strip_html_keeps_text_only() {
        assert_eq!(strip_html("<p>a</p><p>b</p>"), "a\nb");
    }

    #[test]
    fn strip_html_extracts_blk_paragraphs() {
        let html = r#"<header></header><article><h1><blk>标题</blk></h1><p><blk e="1">第一段。</blk></p><p></p><p><blk e="2">第二段。</blk></p></article>"#;
        // <h1> 里的 blk 是标题，由上层单独写，正文不应重复它。
        assert_eq!(strip_html(html), "第一段。\n第二段。");
    }

    #[test]
    fn strip_html_unescapes_entities() {
        assert_eq!(strip_html("<blk>a&amp;b&nbsp;c</blk>"), "a&b c");
    }
}
