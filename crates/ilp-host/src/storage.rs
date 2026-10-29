use anyhow::{Context, Result};
use ilp_types::ChapterEntry;
use rusqlite::{params, Connection, OptionalExtension};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct Book {
    pub title: String,
    pub author: String,
    pub cover_url: String,
    pub chapters: Vec<ChapterEntry>,
}

/// 章节落盘状态的唯一事实来源。
///
/// 只看文件是否存在是不够的：写到一半崩溃会留下一个非空但残缺的文件，
/// 下次启动就被当成“已下载”永久跳过。所以这里记住写入时的字节数，
/// 续传时拿它和磁盘上的实际长度比对。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChapterState {
    pub path: PathBuf,
    pub bytes: u64,
    pub sha256: String,
}

pub struct BookLock {
    path: PathBuf,
}

impl Drop for BookLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
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
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             CREATE TABLE IF NOT EXISTS books (
                site_id    TEXT NOT NULL,
                book_id    TEXT NOT NULL,
                title      TEXT NOT NULL,
                author     TEXT NOT NULL,
                cover_url  TEXT NOT NULL,
                chapters   TEXT NOT NULL,
                updated_at INTEGER NOT NULL,
                PRIMARY KEY (site_id, book_id)
            );
             CREATE TABLE IF NOT EXISTS chapters (
                site_id    TEXT NOT NULL,
                book_id    TEXT NOT NULL,
                chapter_id TEXT NOT NULL,
                path       TEXT NOT NULL,
                status     TEXT NOT NULL,
                bytes      INTEGER NOT NULL,
                sha256     TEXT NOT NULL DEFAULT '',
                mtime      INTEGER NOT NULL,
                PRIMARY KEY (site_id, book_id, chapter_id)
             );",
        )?;
        let _ = db.execute(
            "ALTER TABLE chapters ADD COLUMN sha256 TEXT NOT NULL DEFAULT ''",
            [],
        );
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
        let now = now_secs();
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

    /// 一本书的目录。带 book_id 是为了避免同名书互相覆盖。
    pub fn book_dir(&self, book_title: &str, book_id: &str) -> PathBuf {
        self.novels_dir
            .join(format!("{}_{}", sanitize(book_title), sanitize(book_id)))
    }

    /// 章节文件带序号前缀，保证文件管理器里的顺序与阅读顺序一致。
    /// 纯按标题排序会把「第10章」排到「第2章」前面。
    pub fn chapter_path(
        &self,
        book_title: &str,
        book_id: &str,
        seq: usize,
        chapter_title: &str,
    ) -> PathBuf {
        self.book_dir(book_title, book_id).join(format!(
            "{:04}_{}.txt",
            seq,
            sanitize(chapter_title)
        ))
    }

    /// 封面扩展名按魔数判定：番茄返回的是 JPEG，原版一律存 .png 会得到
    /// 扩展名与内容不符的文件。
    pub fn cover_path_for(&self, book_title: &str, book_id: &str, data: &[u8]) -> PathBuf {
        let ext = sniff_ext(data);
        self.covers_dir.join(format!(
            "{}_{}.{ext}",
            sanitize(book_title),
            sanitize(book_id)
        ))
    }

    /// 已有封面（任意扩展名）就返回它的路径。
    pub fn existing_cover(&self, book_title: &str, book_id: &str) -> Option<PathBuf> {
        for ext in ["jpg", "png", "webp", "gif", "bin"] {
            let p = self.covers_dir.join(format!(
                "{}_{}.{ext}",
                sanitize(book_title),
                sanitize(book_id)
            ));
            if p.exists() {
                return Some(p);
            }
        }
        None
    }

    fn chapter_state(
        &self,
        site_id: &str,
        book_id: &str,
        chapter_id: &str,
    ) -> Result<Option<ChapterState>> {
        let row = self
            .db
            .prepare(
                "SELECT path, bytes, sha256 FROM chapters
                 WHERE site_id = ?1 AND book_id = ?2 AND chapter_id = ?3 AND status = 'ok'",
            )?
            .query_row(params![site_id, book_id, chapter_id], |r| {
                Ok(ChapterState {
                    path: PathBuf::from(r.get::<_, String>(0)?),
                    bytes: r.get::<_, i64>(1)? as u64,
                    sha256: r.get::<_, String>(2)?,
                })
            })
            .optional()?;
        Ok(row)
    }

    /// 真正的续传判断：DB 里有成功记录、文件还在、且长度与记录一致。
    /// 任一不满足都视为需要重新下载。
    pub fn is_chapter_downloaded(&self, site_id: &str, book_id: &str, chapter_id: &str) -> bool {
        let Ok(Some(state)) = self.chapter_state(site_id, book_id, chapter_id) else {
            return false;
        };
        match std::fs::metadata(&state.path) {
            Ok(meta) if meta.is_file() && meta.len() == state.bytes => {
                state.sha256.is_empty()
                    || file_sha256(&state.path)
                        .map(|hash| hash == state.sha256)
                        .unwrap_or(false)
            }
            _ => false,
        }
    }

    /// 原子落盘：先写同目录下的唯一临时文件，fsync 后再 rename 覆盖。
    /// 中途崩溃只会留下 .tmp，不会污染正式文件，也不会被误判为已完成。
    ///
    /// 成功后才写 DB 记录，且记录的是实际写入的字节数；
    /// 顺序很关键——反过来的话，崩溃会留下“记录说完成、文件却是空的”状态。
    #[allow(clippy::too_many_arguments)]
    pub fn save_chapter(
        &self,
        site_id: &str,
        book_title: &str,
        book_id: &str,
        seq: usize,
        chapter_id: &str,
        chapter_title: &str,
        content: &str,
    ) -> Result<PathBuf> {
        let path = self.chapter_path(book_title, book_id, seq, chapter_title);
        atomic_write(&path, content.as_bytes())?;
        let digest = sha256_bytes(content.as_bytes());
        self.record_chapter(
            site_id,
            book_id,
            chapter_id,
            &path,
            content.len() as u64,
            &digest,
        )?;
        Ok(path)
    }

    fn record_chapter(
        &self,
        site_id: &str,
        book_id: &str,
        chapter_id: &str,
        path: &Path,
        bytes: u64,
        sha256: &str,
    ) -> Result<()> {
        self.db.execute(
            "INSERT INTO chapters
                 (site_id, book_id, chapter_id, path, status, bytes, sha256, mtime)
             VALUES (?1, ?2, ?3, ?4, 'ok', ?5, ?6, ?7)
             ON CONFLICT(site_id, book_id, chapter_id) DO UPDATE SET
                 path   = excluded.path,
                 status = excluded.status,
                 bytes  = excluded.bytes,
                 sha256 = excluded.sha256,
                 mtime  = excluded.mtime",
            params![
                site_id,
                book_id,
                chapter_id,
                path.to_string_lossy().to_string(),
                bytes as i64,
                sha256,
                now_secs()
            ],
        )?;
        Ok(())
    }

    /// 记一条失败记录，便于下次只重试这些章，也避免把失败伪装成完成。
    pub fn mark_chapter_failed(
        &self,
        site_id: &str,
        book_id: &str,
        chapter_id: &str,
    ) -> Result<()> {
        self.db.execute(
            "INSERT INTO chapters
                 (site_id, book_id, chapter_id, path, status, bytes, mtime)
             VALUES (?1, ?2, ?3, '', 'failed', 0, ?4)
             ON CONFLICT(site_id, book_id, chapter_id) DO UPDATE SET
                 status = 'failed',
                 bytes  = 0,
                 mtime  = excluded.mtime",
            params![site_id, book_id, chapter_id, now_secs()],
        )?;
        Ok(())
    }

    pub fn failed_chapter_ids(&self, site_id: &str, book_id: &str) -> Result<Vec<String>> {
        let mut stmt = self.db.prepare(
            "SELECT chapter_id FROM chapters
             WHERE site_id = ?1 AND book_id = ?2 AND status = 'failed'",
        )?;
        let rows = stmt
            .query_map(params![site_id, book_id], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn lock_book(&self, site_id: &str, book_id: &str) -> Result<BookLock> {
        let lock_dir = self.novels_dir.join(".locks");
        std::fs::create_dir_all(&lock_dir)?;
        let path = lock_dir.join(format!("{}_{}.lock", sanitize(site_id), sanitize(book_id)));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                writeln!(file, "pid={}", std::process::id())?;
                Ok(BookLock { path })
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(anyhow::anyhow!(
                "这本书正在被另一个进程下载: {site_id}/{book_id}"
            )),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save_cover(&self, book_title: &str, book_id: &str, data: &[u8]) -> Result<PathBuf> {
        let path = self.cover_path_for(book_title, book_id, data);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        atomic_write(&path, data)?;
        Ok(path)
    }
}

/// 写临时文件 → fsync → rename。rename 在同一文件系统内是原子的，
/// 所以正式路径要么不存在，要么是完整内容。
fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .with_context(|| format!("路径没有父目录: {}", path.display()))?;
    std::fs::create_dir_all(dir)?;

    let tmp = unique_tmp(dir, path);
    {
        let mut f = std::fs::File::create(&tmp)
            .with_context(|| format!("创建临时文件失败: {}", tmp.display()))?;
        f.write_all(data)?;
        f.flush()?;
        f.sync_all()?;
    }

    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e).with_context(|| format!("重命名到 {} 失败", path.display()))
        }
    }
}

/// 临时文件名带进程号与纳秒计数，避免并发章节互相覆盖同一个 `.tmp`。
fn unique_tmp(dir: &Path, target: &Path) -> PathBuf {
    let stem = target
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "chapter".to_string());
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    dir.join(format!(".{stem}.{}.{nanos}.tmp", std::process::id()))
}

fn sha256_bytes(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(data))
}

fn file_sha256(path: &Path) -> std::io::Result<String> {
    let data = std::fs::read(path)?;
    Ok(sha256_bytes(&data))
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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

/// 清洗成合法文件名。除了 Windows 非法字符，还要防 `..` 之类的路径穿越。
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
    if trimmed.is_empty() || trimmed.chars().all(|c| c == '.') {
        "untitled".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let p = std::env::temp_dir().join(format!("ilp-storage-{tag}-{nanos}"));
            std::fs::create_dir_all(&p).expect("创建临时目录");
            Self(p)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn storage_in(dir: &Path) -> Storage {
        Storage::open(
            dir.join("cache.db").to_str().expect("临时路径应是 UTF-8"),
            dir.join("novels"),
            dir.join("covers"),
        )
        .expect("打开存储")
    }

    fn write_chapter(s: &Storage, content: &str) -> PathBuf {
        s.save_chapter("fanqie", "书", "42", 1, "ch1", "第1章", content)
            .expect("写入章节")
    }

    #[test]
    fn chapter_path_includes_book_id_and_seq() {
        let dir = TempDir::new("path");
        let s = storage_in(&dir.0);
        let p = s.chapter_path("部族荣光", "7069", 7, "第7章 起");
        let text = p.to_string_lossy().to_string();
        assert!(text.contains("部族荣光_7069"), "带上 book_id：{text}");
        assert!(text.contains("0007_"), "序号补零到四位：{text}");
    }

    #[test]
    fn resume_accepts_matching_size() {
        let dir = TempDir::new("resume-ok");
        let s = storage_in(&dir.0);
        write_chapter(&s, "正文正文");
        assert!(s.is_chapter_downloaded("fanqie", "42", "ch1"));
    }

    #[test]
    fn resume_rejects_truncated_file() {
        let dir = TempDir::new("resume-truncated");
        let s = storage_in(&dir.0);
        let path = write_chapter(&s, "一二三四五六七八九十");
        // 模拟写到一半崩溃：文件还在，但内容短了。
        std::fs::write(&path, "一二").expect("截断文件");
        assert!(
            !s.is_chapter_downloaded("fanqie", "42", "ch1"),
            "长度不符必须重新下载"
        );
    }

    #[test]
    fn resume_rejects_missing_file() {
        let dir = TempDir::new("resume-missing");
        let s = storage_in(&dir.0);
        let path = write_chapter(&s, "正文");
        std::fs::remove_file(&path).expect("删除文件");
        assert!(!s.is_chapter_downloaded("fanqie", "42", "ch1"));
    }

    #[test]
    fn resume_rejects_unknown_chapter() {
        let dir = TempDir::new("resume-unknown");
        let s = storage_in(&dir.0);
        assert!(!s.is_chapter_downloaded("fanqie", "42", "nope"));
    }

    #[test]
    fn failed_chapter_is_not_counted_as_downloaded() {
        let dir = TempDir::new("failed");
        let s = storage_in(&dir.0);
        write_chapter(&s, "正文");
        s.mark_chapter_failed("fanqie", "42", "ch1")
            .expect("标记失败");
        assert!(!s.is_chapter_downloaded("fanqie", "42", "ch1"));
        assert_eq!(
            s.failed_chapter_ids("fanqie", "42").expect("读取失败列表"),
            vec!["ch1".to_string()]
        );
    }

    #[test]
    fn atomic_write_leaves_no_temp_files() {
        let dir = TempDir::new("no-tmp");
        let s = storage_in(&dir.0);
        write_chapter(&s, "正文");
        let entries: Vec<_> = std::fs::read_dir(s.book_dir("书", "42"))
            .expect("读目录")
            .map(|e| e.expect("条目").file_name().to_string_lossy().to_string())
            .collect();
        assert!(
            entries.iter().all(|n| !n.ends_with(".tmp")),
            "不应残留临时文件: {entries:?}"
        );
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn books_round_trip() {
        let dir = TempDir::new("book");
        let s = storage_in(&dir.0);
        let book = Book {
            title: "书".to_string(),
            author: "作者".to_string(),
            cover_url: "u".to_string(),
            chapters: vec![ChapterEntry {
                id: "ch1".to_string(),
                title: "第1章".to_string(),
                url: "a".to_string(),
                fetch_url: "b".to_string(),
                update_time: 0,
            }],
        };
        s.save_book("fanqie", "42", &book).expect("保存书籍");
        let back = s.load_book("fanqie", "42").expect("读取").expect("应存在");
        assert_eq!(back.title, "书");
        assert_eq!(back.chapters.len(), 1);
        assert_eq!(back.chapters[0].id, "ch1");
    }

    #[test]
    fn sanitize_blocks_traversal_and_illegal_chars() {
        assert_eq!(sanitize("a/b\\c:d*e?f\"g<h>i|j"), "a_b_c_d_e_f_g_h_i_j");
        assert_eq!(sanitize("名字.."), "名字");
        assert_eq!(sanitize("   "), "untitled");
        assert_eq!(sanitize("..."), "untitled");
        assert!(!sanitize("../../etc/passwd").contains('/'));
    }

    #[test]
    fn cover_ext_follows_magic_bytes() {
        let dir = TempDir::new("cover");
        let s = storage_in(&dir.0);
        let jpeg = [0xFF, 0xD8, 0xFF, 0xE0, 0, 0, 0, 0];
        let p = s.save_cover("书", "42", &jpeg).expect("保存封面");
        assert_eq!(p.extension().expect("有扩展名"), "jpg");
        assert!(s.existing_cover("书", "42").is_some());
    }
}
