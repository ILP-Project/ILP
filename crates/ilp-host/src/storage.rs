use anyhow::{Context, Result};
use ilp_types::ChapterEntry;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct Book {
    pub title: String,
    pub author: String,
    pub cover_url: String,
    pub chapters: Vec<ChapterEntry>,
}

pub struct Storage {
    db: Connection,
    novels_dir: PathBuf,
    covers_dir: PathBuf,
}

impl Storage {
    pub fn open(db_path: &str, novels_dir: PathBuf, covers_dir: PathBuf) -> Result<Self> {
        let db = Connection::open(db_path).with_context(|| format!("打开数据库失败: {db_path}"))?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS books (
                site_id    TEXT NOT NULL,
                book_id    TEXT NOT NULL,
                title      TEXT NOT NULL,
                author     TEXT NOT NULL,
                cover_url  TEXT NOT NULL,
                chapters   TEXT NOT NULL,
                updated_at INTEGER NOT NULL,
                PRIMARY KEY (site_id, book_id)
            );",
        )?;
        Ok(Self {
            db,
            novels_dir,
            covers_dir,
        })
    }

    pub fn load_book(&self, site_id: &str, book_id: &str) -> Result<Option<Book>> {
        let mut stmt = self.db.prepare(
            "SELECT title, author, cover_url, chapters FROM books
             WHERE site_id = ?1 AND book_id = ?2",
        )?;
        let row = stmt
            .query_row(params![site_id, book_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })
            .optional()?;

        match row {
            Some((title, author, cover_url, chapters)) => {
                let chapters: Vec<ChapterEntry> = serde_json::from_str(&chapters)?;
                Ok(Some(Book {
                    title,
                    author,
                    cover_url,
                    chapters,
                }))
            }
            None => Ok(None),
        }
    }

    pub fn save_book(&self, site_id: &str, book_id: &str, book: &Book) -> Result<()> {
        let chapters = serde_json::to_string(&book.chapters)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        self.db.execute(
            "INSERT INTO books (site_id, book_id, title, author, cover_url, chapters, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(site_id, book_id) DO UPDATE SET
                 title      = excluded.title,
                 author     = excluded.author,
                 cover_url  = excluded.cover_url,
                 chapters   = excluded.chapters,
                 updated_at = excluded.updated_at",
            params![
                site_id,
                book_id,
                book.title,
                book.author,
                book.cover_url,
                chapters,
                now
            ],
        )?;
        Ok(())
    }

    pub fn chapter_path(&self, book_title: &str, chapter_title: &str) -> PathBuf {
        self.novels_dir
            .join(sanitize(book_title))
            .join(format!("{}.txt", sanitize(chapter_title)))
    }

    /// 封面扩展名按魔数判定：番茄返回的是 JPEG，原版一律存 .png 会得到
    /// 扩展名与内容不符的文件。
    pub fn cover_path_for(&self, book_title: &str, data: &[u8]) -> PathBuf {
        let ext = sniff_ext(data);
        self.covers_dir
            .join(format!("{}.{ext}", sanitize(book_title)))
    }

    /// 已有封面（任意扩展名）就返回它的路径。
    pub fn existing_cover(&self, book_title: &str) -> Option<PathBuf> {
        for ext in ["jpg", "png", "webp", "gif"] {
            let p = self
                .covers_dir
                .join(format!("{}.{ext}", sanitize(book_title)));
            if p.exists() {
                return Some(p);
            }
        }
        None
    }

    pub fn is_chapter_downloaded(&self, book_title: &str, chapter_title: &str) -> bool {
        self.chapter_path(book_title, chapter_title).exists()
    }

    pub fn save_chapter(&self, book_title: &str, chapter_title: &str, content: &str) -> Result<()> {
        let path = self.chapter_path(book_title, chapter_title);
        let dir = path.parent().expect("章节路径无父目录");
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join(format!("{}.tmp", sanitize(chapter_title)));
        std::fs::write(&tmp, content)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn save_cover(&self, book_title: &str, data: &[u8]) -> Result<PathBuf> {
        let path = self.cover_path_for(book_title, data);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, data)?;
        Ok(path)
    }
}

fn sniff_ext(data: &[u8]) -> &'static str {
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "jpg"
    } else if data.starts_with(b"\x89PNG") {
        "png"
    } else if data.len() > 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        "webp"
    } else if data.starts_with(b"GIF8") {
        "gif"
    } else {
        "bin"
    }
}

pub fn sanitize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
            out.push('_');
        } else {
            out.push(c);
        }
    }
    let trimmed = out.trim_end_matches(['.', ' ']);
    if trimmed.is_empty() {
        "untitled".to_string()
    } else {
        trimmed.to_string()
    }
}
