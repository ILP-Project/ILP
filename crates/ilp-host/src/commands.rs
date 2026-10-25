use anyhow::{anyhow, Result};

use crate::config::Config;
use crate::engine::PluginInstance;
use crate::storage::{Book, Storage};
use ilp_types::ChapterContent;

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
            let path = storage.save_cover(&title, &bytes)?;
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

    if with_cover && storage.existing_cover(&book.title).is_none() {
        let bytes = crate::http::get_bytes(&book.cover_url)?;
        storage.save_cover(&book.title, &bytes)?;
    }

    fetch_all(plugin, storage, config, &book)
}

fn fetch_all(
    plugin: &mut PluginInstance,
    storage: &Storage,
    config: &Config,
    book: &Book,
) -> Result<()> {
    let todo: Vec<_> = book
        .chapters
        .iter()
        .filter(|c| !storage.is_chapter_downloaded(&book.title, &c.title))
        .cloned()
        .collect();

    if todo.is_empty() {
        println!("所有章节已下载");
        return Ok(());
    }

    let sleep_ms = config.sleep_time * 1000;
    let mut done = 0usize;
    let mut failed = 0usize;

    for chunk in todo.chunks(BATCH_SIZE) {
        let ids: Vec<String> = chunk.iter().map(|c| c.id.clone()).collect();
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
                retry_missing(plugin, storage, config, book, chunk, &mut done, &mut failed);
                continue;
            };
            let got = match plugin.consume_fetch(&task.id, resp) {
                Ok(items) => items,
                Err(e) => {
                    tracing::warn!("解析响应失败 {}: {e:#}", task.request.url);
                    retry_missing(plugin, storage, config, book, chunk, &mut done, &mut failed);
                    continue;
                }
            };
            let landed: std::collections::HashSet<String> =
                got.iter().map(|i| i.id.clone()).collect();
            for item in &got {
                persist(storage, book, item, &mut done, &mut failed);
            }
            // 服务端限速时批量接口会少返回若干章，必须单独补齐。
            let missing: Vec<_> = chunk
                .iter()
                .filter(|c| !landed.contains(&c.id))
                .cloned()
                .collect();
            if !missing.is_empty() {
                tracing::info!("本批缺 {} 章，逐章重试", missing.len());
                retry_missing(
                    plugin,
                    storage,
                    config,
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
        anyhow::bail!("有 {failed} 章下载失败");
    }
    Ok(())
}

/// 逐章补抓：批接口少返回时退回单章请求，间隔 sleep_time 避开限速。
fn retry_missing(
    plugin: &mut PluginInstance,
    storage: &Storage,
    config: &Config,
    book: &Book,
    chapters: &[ilp_types::ChapterEntry],
    done: &mut usize,
    failed: &mut usize,
) {
    let sleep_ms = config.sleep_time * 1000;
    for ch in chapters {
        std::thread::sleep(std::time::Duration::from_millis(sleep_ms));
        let ids = vec![ch.id.clone()];
        let tasks = match plugin.plan_fetch(&ids) {
            Ok(t) if !t.is_empty() => t,
            _ => {
                tracing::warn!("单章重试计划失败: {}", ch.title);
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
                        persist(storage, book, item, done, failed);
                    }
                }
                _ => {
                    tracing::warn!("单章重试仍失败: {}", ch.title);
                    *failed += 1;
                }
            },
            _ => {
                tracing::warn!("单章重试请求失败: {}", ch.title);
                *failed += 1;
            }
        }
    }
}

fn persist(
    storage: &Storage,
    book: &Book,
    item: &ChapterContent,
    done: &mut usize,
    failed: &mut usize,
) {
    let full = format!("{}\n{}\n---\n{}", item.title, book.author, item.text);
    match storage.save_chapter(&book.title, &item.title, &full) {
        Ok(()) => {
            *done += 1;
            println!("已下载: {} - {}", book.title, item.title);
        }
        Err(e) => {
            tracing::warn!("写入失败: {} - {}: {e:#}", book.title, item.title);
            *failed += 1;
        }
    }
}

pub fn do_search(
    plugins: &mut [PluginInstance],
    _config: &Config,
    keyword: &str,
    site_id: &str,
) -> Result<()> {
    let plugin = find_plugin(plugins, site_id)?;
    let page = plugin.search(keyword, 0)?;
    if page.is_empty() {
        println!("无搜索结果");
    } else {
        for r in page {
            println!("{} - {} - {}", r.id, r.title, r.author);
        }
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
