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
                "无效的获取类型: {kind}（可选 chapter/cover/author/all）"
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
            println!("已下载: {} - {}", book.title, item.title);
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
    _config: &Config,
    keyword: &str,
    site_id: &str,
) -> Result<()> {
    let plugin = find_plugin(plugins, site_id)?;

    if !std::io::stdin().is_terminal() {
        return search_dump_all(plugin, keyword);
    }

    // 用栈保存已访问的页：栈顶是当前页，出栈即回上一页。
    let mut stack: Vec<ilp_types::SearchPage> = Vec::new();

    loop {
        if stack.is_empty() {
            // 首页：cursor 为空。
            stack.push(plugin.search(keyword, "")?);
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
                let next = plugin.search(keyword, &current.cursor)?;
                if next.results.is_empty() {
                    println!("没有更多结果了。");
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

/// 非交互场景：沿游标翻到服务端说没有为止，并设上限防死循环。
fn search_dump_all(plugin: &mut PluginInstance, keyword: &str) -> Result<()> {
    const MAX_PAGES: usize = 50;
    let mut cursor = String::new();
    for _ in 0..MAX_PAGES {
        let page = plugin.search(keyword, &cursor)?;
        for r in &page.results {
            println!("{} - {} - {}", r.id, r.title, r.author);
        }
        if !page.has_more || page.results.is_empty() || page.cursor.is_empty() {
            break;
        }
        cursor = page.cursor;
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use crate::exit;

    use super::next_page;
    use super::PageCmd;

    #[test]
    fn next_page_keys() {
        assert_eq!(next_page("n"), PageCmd::Next);
        assert_eq!(next_page(""), PageCmd::Next);
        assert_eq!(next_page("  N  "), PageCmd::Next);
        assert_eq!(next_page("p"), PageCmd::Prev);
        assert_eq!(next_page("Q"), PageCmd::Quit);
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
