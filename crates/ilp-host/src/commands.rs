use anyhow::{anyhow, Result};

use crate::config::Config;
use crate::engine::PluginInstance;
use crate::storage::{Book, Storage};
use ilp_types::ChapterContent;
use std::io::IsTerminal;

/// 一批最多塞多少章给 plan_fetch。
/// 批量站点用它摊薄请求数，网页站点自己会一章一任务。
const BATCH_SIZE: usize = 30;

pub fn do_get(
    plugins: &mut [PluginInstance],
    config: &Config,
    kind: &str,
    id: &str,
    site_id: &str,
    force_update: bool,
) -> Result<()> {
    let plugin = find_plugin(plugins, site_id)?;
    let storage = Storage::open("cache.db", config.novels_dir(), config.covers_dir())?;
    plugin.init(id, force_update)?;

    match kind {
        "title" => println!("{}", plugin.title()?),
        "author" => println!("{}", plugin.author()?),
        "cover" => {
            let title = plugin.title()?;
            let url = plugin.cover_url()?;
            let bytes = crate::http::get_bytes(&url)?;
            let path = storage.save_cover(&title, id, &bytes)?;
            println!("封面已保存: {}", path.display());
        }
        "chapter" => do_chapters(plugin, &storage, config, site_id, id, force_update, false)?,
        "all" => do_chapters(plugin, &storage, config, site_id, id, force_update, true)?,
        _ => {
            return Err(anyhow!(
                "无效的获取类型: {kind}（可选 chapter/cover/author/title/all）"
            ))
        }
    }
    Ok(())
}

fn do_chapters(
    plugin: &mut PluginInstance,
    storage: &Storage,
    config: &Config,
    site_id: &str,
    book_id: &str,
    force_update: bool,
    with_cover: bool,
) -> Result<()> {
    let cached = if force_update {
        None
    } else {
        storage.load_book(site_id, book_id)?
    };

    let book = match cached {
        Some(b) => b,
        None => {
            let title = plugin.title()?;
            let author = plugin.author()?;
            let cover_url = plugin.cover_url()?;
            if let Ok(summary) = plugin.description() {
                if !summary.is_empty() {
                    tracing::info!("简介: {summary}");
                }
            }
            let chapters = plugin.catalog(force_update)?;
            let b = Book {
                title,
                author,
                cover_url,
                chapters,
            };
            storage.save_book(site_id, book_id, &b)?;
            b
        }
    };

    if with_cover && storage.existing_cover(&book.title, book_id).is_none() {
        let bytes = crate::http::get_bytes(&book.cover_url)?;
        storage.save_cover(&book.title, book_id, &bytes)?;
    }

    let _lock = storage.lock_book(site_id, book_id)?;
    fetch_all(plugin, storage, config, site_id, book_id, &book)
}

fn fetch_all(
    plugin: &mut PluginInstance,
    storage: &Storage,
    config: &Config,
    site_id: &str,
    book_id: &str,
    book: &Book,
) -> Result<()> {
    // 序号来自目录里的原始位置，与是否已下载无关，这样文件名顺序稳定。
    let todo: Vec<(usize, ilp_types::ChapterEntry)> = book
        .chapters
        .iter()
        .enumerate()
        .filter(|(_, c)| !storage.is_chapter_downloaded(site_id, book_id, &c.id))
        .map(|(i, c)| (i + 1, c.clone()))
        .collect();

    if todo.is_empty() {
        println!("所有章节已下载");
        return Ok(());
    }

    let sleep_ms = config.sleep_time * 1000;
    let mut done = 0usize;
    let mut failed = 0usize;

    for chunk in todo.chunks(BATCH_SIZE) {
        let ids: Vec<String> = chunk.iter().map(|(_, c)| c.id.clone()).collect();
        let tasks = match plugin.plan_fetch(&ids) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("生成抓取计划失败（{} 章）: {e:#}", chunk.len());
                failed += chunk.len();
                continue;
            }
        };

        // 宿主负责真正发请求：并发度与限速由配置统一控制。
        let requests: Vec<&crate::engine::wit_types::HttpRequest> =
            tasks.iter().map(|t| &t.request).collect();
        let responses =
            crate::http::fetch_requests(&requests, config.effective_max_workers(), sleep_ms);

        for (task, resp) in tasks.iter().zip(responses) {
            let Ok(resp) = resp else {
                tracing::warn!("请求失败: {}", task.request.url);
                retry_missing(
                    plugin,
                    storage,
                    config,
                    site_id,
                    book_id,
                    book,
                    chunk,
                    &mut done,
                    &mut failed,
                );
                continue;
            };
            let got = match plugin.consume_fetch(&task.id, resp) {
                Ok(items) => items,
                Err(e) => {
                    tracing::warn!("解析响应失败 {}: {e:#}", task.request.url);
                    retry_missing(
                        plugin,
                        storage,
                        config,
                        site_id,
                        book_id,
                        book,
                        chunk,
                        &mut done,
                        &mut failed,
                    );
                    continue;
                }
            };
            let landed: std::collections::HashSet<String> =
                got.iter().map(|i| i.id.clone()).collect();
            for item in &got {
                let seq = chunk
                    .iter()
                    .find(|(_, c)| c.id == item.id)
                    .map(|(s, _)| *s)
                    .unwrap_or(0);
                persist(
                    storage,
                    site_id,
                    book_id,
                    book,
                    seq,
                    item,
                    &mut done,
                    &mut failed,
                );
            }
            // 服务端限速时批量接口会少返回若干章，必须单独补齐。
            let missing: Vec<(usize, ilp_types::ChapterEntry)> = chunk
                .iter()
                .filter(|(_, c)| !landed.contains(&c.id))
                .cloned()
                .collect();
            if !missing.is_empty() {
                tracing::info!("本批缺 {} 章，逐章重试", missing.len());
                retry_missing(
                    plugin,
                    storage,
                    config,
                    site_id,
                    book_id,
                    book,
                    &missing,
                    &mut done,
                    &mut failed,
                );
            }
        }
    }

    println!("完成: 成功 {done} 章，失败 {failed} 章");
    if failed > 0 {
        // 顺带把历史失败也列出来：之前跑到一半失败的章，这次可能根本没进入待办。
        match storage.failed_chapter_ids(site_id, book_id) {
            Ok(ids) if !ids.is_empty() => {
                tracing::warn!("库内累计失败章 {} 个，重跑同一命令会重试", ids.len());
            }
            _ => {}
        }
        return Err(anyhow::Error::new(crate::exit::PartialFailure(format!(
            "有 {failed} 章下载失败"
        ))));
    }
    Ok(())
}

/// 逐章补抓：批接口少返回时退回单章请求，间隔 sleep_time 避开限速。
#[allow(clippy::too_many_arguments)]
fn retry_missing(
    plugin: &mut PluginInstance,
    storage: &Storage,
    config: &Config,
    site_id: &str,
    book_id: &str,
    book: &Book,
    chapters: &[(usize, ilp_types::ChapterEntry)],
    done: &mut usize,
    failed: &mut usize,
) {
    let sleep_ms = config.sleep_time * 1000;
    for (seq, ch) in chapters {
        std::thread::sleep(std::time::Duration::from_millis(sleep_ms));
        let ids = vec![ch.id.clone()];
        let tasks = match plugin.plan_fetch(&ids) {
            Ok(t) if !t.is_empty() => t,
            _ => {
                tracing::warn!("单章重试计划失败: {}", ch.title);
                mark_failed(storage, site_id, book_id, &ch.id);
                *failed += 1;
                continue;
            }
        };
        let requests: Vec<&crate::engine::wit_types::HttpRequest> =
            tasks.iter().map(|t| &t.request).collect();
        let responses = crate::http::fetch_requests(&requests, 1, 0);
        match responses.into_iter().next() {
            Some(Ok(resp)) => match plugin.consume_fetch(&tasks[0].id, resp) {
                Ok(items) if !items.is_empty() => {
                    for item in &items {
                        persist(storage, site_id, book_id, book, *seq, item, done, failed);
                    }
                }
                _ => {
                    tracing::warn!("单章重试仍失败: {}", ch.title);
                    mark_failed(storage, site_id, book_id, &ch.id);
                    *failed += 1;
                }
            },
            _ => {
                tracing::warn!("单章重试请求失败: {}", ch.title);
                mark_failed(storage, site_id, book_id, &ch.id);
                *failed += 1;
            }
        }
    }
}

fn mark_failed(storage: &Storage, site_id: &str, book_id: &str, chapter_id: &str) {
    if let Err(e) = storage.mark_chapter_failed(site_id, book_id, chapter_id) {
        tracing::warn!("记录失败状态出错: {e:#}");
    }
}

#[allow(clippy::too_many_arguments)]
fn persist(
    storage: &Storage,
    site_id: &str,
    book_id: &str,
    book: &Book,
    seq: usize,
    item: &ChapterContent,
    done: &mut usize,
    failed: &mut usize,
) {
    let full = format!("{}\n{}\n---\n{}", item.title, book.author, item.text);
    match storage.save_chapter(
        site_id,
        &book.title,
        book_id,
        seq,
        &item.id,
        &item.title,
        &full,
    ) {
        Ok(_) => {
            *done += 1;
            if std::env::var_os("ILP_JSONL").is_some() {
                let event = serde_json::json!({"event":"chapter_downloaded","book":book.title,"chapter":item.title,"id":item.id});
                println!("{}", event);
            } else {
                println!("已下载: {} - {}", book.title, item.title);
            }
        }
        Err(e) => {
            tracing::warn!("写入失败: {} - {}: {e:#}", book.title, item.title);
            mark_failed(storage, site_id, book_id, &item.id);
            *failed += 1;
        }
    }
}

/// 搜索结果分页浏览。
///
/// 番茄的搜索接口是游标式的：offset 会被服务端直接忽略，只有把上一页返回
/// 的 cursor（passback + search_id）原样带回去才能翻到下一页。因此这里不提供
/// "跳到第 N 页" —— 游标模型下要么做不到，要么得偷偷连发 N-1 个请求。
///
/// 交互键：n 或直接回车下一页、p 上一页、q 退出。上一页走缓存不重新请求。
/// 非终端（管道、重定向）不阻塞等输入，改为把结果全部打印出来。
pub fn do_search(
    plugins: &mut [PluginInstance],
    config: &Config,
    keyword: &str,
    site_id: &str,
) -> Result<()> {
    let plugin = find_plugin(plugins, site_id)?;
    let sleep_ms = config.sleep_time.saturating_mul(1000);

    if !std::io::stdin().is_terminal() {
        return search_dump_all(plugin, keyword, sleep_ms);
    }

    // 用栈保存已访问的页：栈顶是当前页，出栈即回上一页。
    let mut stack: Vec<ilp_types::SearchPage> = Vec::new();
    let mut last_req: Option<std::time::Instant> = None;

    loop {
        if stack.is_empty() {
            // 首页：cursor 为空。
            stack.push(plugin.search(keyword, "")?);
            last_req = Some(std::time::Instant::now());
        }
        let Some(current) = stack.last().cloned() else {
            return Ok(());
        };
        print_search_page(stack.len(), &current);

        let input = read_line("· n 下一页 / p 上一页 / q 退出 > ")?;
        match next_page(&input) {
            PageCmd::Quit => return Ok(()),
            PageCmd::Next => {
                if !current.has_more {
                    println!("已经是最后一页。");
                    continue;
                }
                // 搜索走插件的同步 http-fetch，绕过了宿主的并发限速器，
                // 所以这里要自己维持页间间隔，否则连翻会被静默限流。
                throttle(last_req, sleep_ms);
                let next = plugin.search(keyword, &current.cursor)?;
                last_req = Some(std::time::Instant::now());
                if next.results.is_empty() {
                    // 服务端限流时会 has_more=true 却给空页。这时说"没有更多"
                    // 是在骗人——结果确实还有，只是这次没拿到。
                    if next.has_more {
                        println!("服务端返回了空页（可能被限流），稍后按 n 重试。");
                    } else {
                        println!("没有更多结果了。");
                    }
                    continue;
                }
                stack.push(next);
            }
            PageCmd::Prev => {
                if stack.len() <= 1 {
                    println!("已经是第一页。");
                    continue;
                }
                stack.pop();
            }
            PageCmd::Note(msg) => println!("{msg}"),
        }
    }
}

/// 翻页指令。
#[derive(Debug, PartialEq, Eq)]
enum PageCmd {
    Quit,
    Next,
    Prev,
    /// 无法翻页时给用户的提示
    Note(&'static str),
}

/// 纯函数：只有 下一页 / 上一页 / 退出 三种有效指令。
fn next_page(input: &str) -> PageCmd {
    match input.trim().to_ascii_lowercase().as_str() {
        "q" => PageCmd::Quit,
        "" | "n" => PageCmd::Next,
        "p" => PageCmd::Prev,
        _ => PageCmd::Note("无法识别。可用: n 下一页 / p 上一页 / q 退出"),
    }
}

fn print_search_page(page_no: usize, data: &ilp_types::SearchPage) {
    println!(
        "\n=== 第 {} 页（{} 条）{} ===",
        page_no,
        data.results.len(),
        if data.has_more {
            " · 还有更多"
        } else {
            " · 已到末页"
        }
    );
    for r in &data.results {
        println!("{} - {} - {}", r.id, r.title, r.author);
        if !r.description.is_empty() {
            println!("    {}", truncate(&r.description, 60));
        }
    }
}

fn truncate(s: &str, max_chars: usize) -> String {
    let mut out: String = s.chars().take(max_chars).collect();
    if s.chars().count() > max_chars {
        out.push('…');
    }
    out
}

fn read_line(prompt: &str) -> Result<String> {
    use std::io::Write;
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut buf = String::new();
    std::io::stdin().read_line(&mut buf)?;
    Ok(buf)
}

/// 两次请求之间补足最小间隔。
fn throttle(last: Option<std::time::Instant>, sleep_ms: u64) {
    if sleep_ms == 0 {
        return;
    }
    if let Some(prev) = last {
        let gap = std::time::Duration::from_millis(sleep_ms);
        let elapsed = prev.elapsed();
        if elapsed < gap {
            std::thread::sleep(gap - elapsed);
        }
    }
}

/// 非交互场景：沿游标翻到服务端说没有为止，并设上限防死循环。
///
/// 服务端在限流时会给出 has_more=true 的空页。这种情况不能当作"结果没了"
/// ——那会把一次限流伪装成正常结束（实测同一关键词会在 49 和 148 条之间跳动）。
/// 空页先重试几次，仍然不行就以 partial 收尾，让调用方知道结果不完整。
/// 一页结果的处置。抽成纯函数是为了能把"空页不等于结束"这条规则钉死在
/// 单元测试里——服务端限流时正是靠它避免把不完整的结果当成正常收尾。
#[derive(Debug, PartialEq, Eq)]
enum PageStep {
    /// 没有更多了，正常结束
    Done,
    /// 继续翻，带上下一个游标
    Next(String),
    /// 还有更多但这次没拿到：重试同一游标
    Retry,
    /// 重试次数用尽，结果不完整
    GiveUp,
}

fn page_step(page: &ilp_types::SearchPage, empty_retries: u32, max_empty_retries: u32) -> PageStep {
    if !page.has_more {
        return PageStep::Done;
    }
    if page.results.is_empty() || page.cursor.is_empty() {
        return if empty_retries >= max_empty_retries {
            PageStep::GiveUp
        } else {
            PageStep::Retry
        };
    }
    PageStep::Next(page.cursor.clone())
}

fn search_dump_all(plugin: &mut PluginInstance, keyword: &str, sleep_ms: u64) -> Result<()> {
    const MAX_PAGES: usize = 50;
    const MAX_EMPTY_RETRIES: u32 = 3;
    let mut cursor = String::new();
    let mut pages = 0usize;
    let mut empty_retries = 0u32;
    let mut last: Option<std::time::Instant> = None;

    loop {
        if pages >= MAX_PAGES {
            return Err(anyhow::Error::new(crate::exit::PartialFailure(format!(
                "搜索「{keyword}」超过 {MAX_PAGES} 页上限，结果可能不完整"
            ))));
        }
        pages += 1;
        throttle(last, sleep_ms);
        let page = plugin.search(keyword, &cursor)?;
        last = Some(std::time::Instant::now());

        for r in &page.results {
            println!("{} - {} - {}", r.id, r.title, r.author);
        }

        match page_step(&page, empty_retries, MAX_EMPTY_RETRIES) {
            PageStep::Done => return Ok(()),
            PageStep::Next(next) => {
                empty_retries = 0;
                cursor = next;
            }
            PageStep::Retry => empty_retries += 1,
            PageStep::GiveUp => {
                return Err(anyhow::Error::new(crate::exit::PartialFailure(format!(
                    "搜索「{keyword}」在翻页时被服务端限流，结果不完整"
                ))));
            }
        }
    }
}

fn find_plugin<'a>(
    plugins: &'a mut [PluginInstance],
    site_id: &str,
) -> Result<&'a mut PluginInstance> {
    plugins
        .iter_mut()
        .find(|p| p.site_id() == site_id)
        .ok_or_else(|| anyhow!("未找到 site_id 为 {site_id} 的已加载插件"))
}

/// `ilp version`：打印宿主版本，并尽力列出每个已加载插件的元数据。
///
/// 单个插件加载失败不影响其余插件的展示——原版一个坏插件会拖垮整个启动。
pub fn do_version(config: &Config) -> Result<()> {
    println!("ilp {}", env!("CARGO_PKG_VERSION"));

    let plugins = match crate::engine::load_plugins(config) {
        Ok(p) => p,
        Err(e) => {
            println!("插件加载失败: {e:#}");
            return Ok(());
        }
    };
    if plugins.is_empty() {
        println!("没有已加载的插件");
        return Ok(());
    }
    println!("已加载插件:");
    for p in &plugins {
        let m = &p.metadata;
        println!(
            "  {} v{} (site: {} / {}) {}",
            m.plugin_name,
            m.version,
            m.site_id,
            m.site_domain,
            if m.plugin_id.is_empty() {
                ""
            } else {
                &m.plugin_id
            }
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::exit;

    use super::next_page;
    use super::page_step;
    use super::PageCmd;
    use super::PageStep;

    #[test]
    fn next_page_keys() {
        assert_eq!(next_page("n"), PageCmd::Next);
        assert_eq!(next_page(""), PageCmd::Next);
        assert_eq!(next_page("  N  "), PageCmd::Next);
        assert_eq!(next_page("p"), PageCmd::Prev);
        assert_eq!(next_page("Q"), PageCmd::Quit);
    }

    fn page(results: &[(&str, &str)], cursor: &str, has_more: bool) -> ilp_types::SearchPage {
        ilp_types::SearchPage {
            results: results
                .iter()
                .map(|(id, title)| ilp_types::SearchResult {
                    id: id.to_string(),
                    title: title.to_string(),
                    author: String::new(),
                    cover_url: String::new(),
                    description: String::new(),
                })
                .collect(),
            cursor: cursor.to_string(),
            has_more,
        }
    }

    #[test]
    fn empty_page_with_more_is_not_treated_as_end() {
        // 这是实测到的服务端行为：限流时 has_more=true 却给空 data。
        // 当成"没有更多"会把一次限流伪装成正常结束。
        let p = page(&[], "cur", true);
        assert_eq!(page_step(&p, 0, 3), PageStep::Retry);
        assert_eq!(page_step(&p, 1, 3), PageStep::Retry);
        assert_eq!(page_step(&p, 2, 3), PageStep::Retry);
        assert_eq!(page_step(&p, 3, 3), PageStep::GiveUp);
    }

    #[test]
    fn empty_page_without_more_is_end() {
        assert_eq!(page_step(&page(&[], "cur", false), 0, 3), PageStep::Done);
    }

    #[test]
    fn page_with_results_advances_by_cursor() {
        let p = page(&[("1", "a")], "next-cur", true);
        assert_eq!(page_step(&p, 0, 3), PageStep::Next("next-cur".to_string()));
    }

    #[test]
    fn page_without_cursor_retries_even_with_results() {
        // 有结果却没游标：翻不了下一页，按限流处理而不是误报结束。
        let p = page(&[("1", "a")], "", true);
        assert_eq!(page_step(&p, 0, 3), PageStep::Retry);
    }

    #[test]
    fn last_page_with_results_is_done() {
        let p = page(&[("1", "a")], "cur", false);
        assert_eq!(page_step(&p, 0, 3), PageStep::Done);
    }

    #[test]
    fn unknown_key_is_rejected_with_hint() {
        // 数字跳页在游标模型下不成立，必须明确拒绝而不是静默忽略。
        assert!(matches!(next_page("5"), PageCmd::Note(_)));
        assert!(matches!(next_page("abc"), PageCmd::Note(_)));
    }

    #[test]
    fn partial_failure_exit_code_is_one() {
        let err = anyhow::Error::new(exit::PartialFailure("有 2 章下载失败".to_string()));
        assert_eq!(exit::code_for(&err), exit::PARTIAL);
        assert_eq!(exit::code_for(&anyhow::anyhow!("致命")), exit::FATAL);
    }
}
