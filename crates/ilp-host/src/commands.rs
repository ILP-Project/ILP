use anyhow::{anyhow, Result};

use crate::config::Config;
use crate::engine::PluginInstance;
use crate::storage::{Book, Storage};

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
        "title" => println!("{}", plugin.get_title()?),
        "author" => println!("{}", plugin.get_author()?),
        "cover" => {
            let title = plugin.get_title()?;
            let url = plugin.get_cover()?;
            let bytes = crate::http::get_bytes(&url)?;
            storage.save_cover(&title, &bytes)?;
            println!("封面已保存: {}", storage.cover_path(&title).display());
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
            let title = plugin.get_title()?;
            let author = plugin.get_author()?;
            let cover_url = plugin.get_cover()?;
            let chapters = plugin.get_content(force_update)?;
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

    if with_cover {
        let path = storage.cover_path(&book.title);
        if !path.exists() {
            let bytes = crate::http::get_bytes(&book.cover_url)?;
            storage.save_cover(&book.title, &bytes)?;
        }
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
        .collect();

    if todo.is_empty() {
        println!("所有章节已下载");
        return Ok(());
    }

    let urls: Vec<String> = todo.iter().map(|c| c.fetch_url.clone()).collect();
    let sleep_ms = config.sleep_time * 1000;
    let results = crate::http::fetch_many(&urls, config.max_workers, sleep_ms);

    for (c, res) in todo.iter().zip(results) {
        match res {
            Ok(body) => {
                let raw = String::from_utf8_lossy(&body).into_owned();
                let text = plugin.parse_chapter(&c.title, &raw)?;
                let full = format!("{}\n{}\n---\n{}", c.title, book.author, text);
                storage.save_chapter(&book.title, &c.title, &full)?;
                println!("已下载: {} - {}", book.title, c.title);
            }
            Err(e) => {
                tracing::warn!("下载失败: {} - {}: {:#}", book.title, c.title, e);
            }
        }
    }
    Ok(())
}

pub fn do_search(plugins: &mut [PluginInstance], keyword: &str, site_id: &str) -> Result<()> {
    let plugin = find_plugin(plugins, site_id)?;
    let results = plugin.search(keyword)?;
    if results.is_empty() {
        println!("无搜索结果");
    } else {
        for r in results {
            println!("{} - {} - {}", r.id, r.title, r.url);
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
        .find(|p| p.site_id == site_id)
        .ok_or_else(|| anyhow!("未找到 site_id 为 {site_id} 的已加载插件"))
}
