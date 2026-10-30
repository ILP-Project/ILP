use anyhow::{Context, Result};
use ilp_types::ChapterEntry;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
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

const MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Manifest {
    schema_version: u32,
    site_id: String,
    book_id: String,
    title: String,
    author: String,
    cover_url: String,
    chapters: Vec<ManifestChapter>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManifestChapter {
    id: String,
    title: String,
    sequence: usize,
    status: String,
    path: Option<String>,
    bytes: u64,
    sha256: String,
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
        self.write_manifest(site_id, book_id, book, &[])?;
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

    fn manifest_path(&self, site_id: &str, book_id: &str) -> PathBuf {
        self.novels_dir.join(format!(
            "{}_{}.manifest.json",
            sanitize(site_id),
            sanitize(book_id)
        ))
    }

    /// 读取并校验 manifest。损坏的 manifest 必须被识别出来，
    /// 否则它会被当作可信的审计副本一直留在磁盘上。
    fn read_manifest(&self, site_id: &str, book_id: &str) -> Result<Option<Manifest>> {
        let path = self.manifest_path(site_id, book_id);
        let data = match std::fs::read(&path) {
            Ok(data) => data,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("读取 {} 失败", path.display())),
        };
        let manifest: Manifest = serde_json::from_slice(&data)
            .with_context(|| format!("命中损坏的 manifest: {}", path.display()))?;
        if manifest.schema_version != MANIFEST_VERSION {
            anyhow::bail!(
                "manifest {} 的 schema_version={} 不受支持",
                path.display(),
                manifest.schema_version
            );
        }
        Ok(Some(manifest))
    }

    /// 校验 manifest，损坏时改名保存并从数据库重建。
    ///
    /// 直接覆盖会丢掉唯一的现场证据；保留 `.corrupt` 才能事后判断
    /// 是磁盘故障、并发写，还是序列化格式变更。
    pub fn repair_manifest(&self, site_id: &str, book_id: &str) -> Result<bool> {
        match self.read_manifest(site_id, book_id) {
            Ok(_) => Ok(false),
            Err(err) => {
                let path = self.manifest_path(site_id, book_id);
                if path.exists() {
                    let backup = corrupt_path(&path);
                    std::fs::rename(&path, &backup).with_context(|| {
                        format!(
                            "保留损坏 manifest 失败: {} -> {}",
                            path.display(),
                            backup.display()
                        )
                    })?;
                    tracing::warn!(
                        manifest = %path.display(),
                        backup = %backup.display(),
                        error = %err,
                        "manifest 损坏，已改名保留并从数据库重建"
                    );
                }
                self.rebuild_manifest(site_id, book_id)?;
                Ok(true)
            }
        }
    }

    fn write_manifest(
        &self,
        site_id: &str,
        book_id: &str,
        book: &Book,
        chapters: &[ManifestChapter],
    ) -> Result<()> {
        let manifest = Manifest {
            schema_version: MANIFEST_VERSION,
            site_id: site_id.to_string(),
            book_id: book_id.to_string(),
            title: book.title.clone(),
            author: book.author.clone(),
            cover_url: book.cover_url.clone(),
            chapters: chapters.to_vec(),
        };
        let data = serde_json::to_vec_pretty(&manifest)?;
        atomic_write(&self.manifest_path(site_id, book_id), &data)
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
    ///
    /// 文件存在但长度或摘要对不上，说明上一次写入被截断或内容被外部改过。
    /// 直接覆盖会把现场抹掉，因此先改名成 `.corrupt` 保留证据再重下。
    pub fn is_chapter_downloaded(&self, site_id: &str, book_id: &str, chapter_id: &str) -> bool {
        let Ok(Some(state)) = self.chapter_state(site_id, book_id, chapter_id) else {
            return false;
        };
        match std::fs::metadata(&state.path) {
            Ok(meta) if meta.is_file() && meta.len() == state.bytes => {
                if state.sha256.is_empty()
                    || file_sha256(&state.path)
                        .map(|hash| hash == state.sha256)
                        .unwrap_or(false)
                {
                    true
                } else {
                    self.quarantine_chapter(site_id, book_id, chapter_id, &state, "摘要不符")
                }
            }
            Ok(meta) if meta.is_file() => self.quarantine_chapter(
                site_id,
                book_id,
                chapter_id,
                &state,
                &format!("长度不符（记录 {}，实际 {}）", state.bytes, meta.len()),
            ),
            _ => false,
        }
    }

    /// 把损坏的章节文件改名保留，并清掉它的 DB 记录。
    /// 清记录是必要的：不清的话下次仍会读到同一条坏记录，反复触发隔离。
    fn quarantine_chapter(
        &self,
        site_id: &str,
        book_id: &str,
        chapter_id: &str,
        state: &ChapterState,
        reason: &str,
    ) -> bool {
        let backup = corrupt_path(&state.path);
        if let Err(err) = std::fs::rename(&state.path, &backup) {
            tracing::warn!(
                path = %state.path.display(),
                error = %err,
                "章节文件损坏且无法隔离，仍将重新下载"
            );
        } else {
            tracing::warn!(
                chapter = chapter_id,
                path = %state.path.display(),
                backup = %backup.display(),
                reason = reason,
                "章节文件已损坏，已改名保留并重新下载"
            );
        }
        if let Err(err) = self.db.execute(
            "DELETE FROM chapters WHERE site_id = ?1 AND book_id = ?2 AND chapter_id = ?3",
            params![site_id, book_id, chapter_id],
        ) {
            tracing::warn!(chapter = chapter_id, error = %err, "删除损坏章节记录失败");
        }
        false
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
        // 重下成功后清掉上一次留下的损坏副本，避免同一章长期堆两份文件。
        remove_corrupt_copies(&path);
        let digest = sha256_bytes(content.as_bytes());
        self.record_chapter(
            site_id,
            book_id,
            chapter_id,
            &path,
            content.len() as u64,
            &digest,
        )?;
        self.rebuild_manifest(site_id, book_id)?;
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
        self.rebuild_manifest(site_id, book_id)?;
        Ok(())
    }

    fn rebuild_manifest(&self, site_id: &str, book_id: &str) -> Result<()> {
        let Some(book) = self.load_book(site_id, book_id)? else {
            return Ok(());
        };
        let mut stmt = self.db.prepare("SELECT chapter_id, path, status, bytes, sha256 FROM chapters WHERE site_id = ?1 AND book_id = ?2")?;
        let states = stmt
            .query_map(params![site_id, book_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)? as u64,
                    row.get::<_, String>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let chapters = book
            .chapters
            .iter()
            .enumerate()
            .map(|(index, chapter)| {
                let state = states.iter().find(|s| s.0 == chapter.id);
                ManifestChapter {
                    id: chapter.id.clone(),
                    title: chapter.title.clone(),
                    sequence: index + 1,
                    status: state
                        .map(|s| s.2.clone())
                        .unwrap_or_else(|| "pending".into()),
                    path: state.and_then(|s| (!s.1.is_empty()).then(|| s.1.clone())),
                    bytes: state.map(|s| s.3).unwrap_or(0),
                    sha256: state.map(|s| s.4.clone()).unwrap_or_default(),
                }
            })
            .collect::<Vec<_>>();
        self.write_manifest(site_id, book_id, &book, &chapters)
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

/// 损坏文件的保留路径：在原文件名后追加 `.corrupt`（保留原扩展名）。
/// 已存在同名备份时追加纳秒时间戳，绝不覆盖上一份证据。
fn corrupt_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let base = path.with_file_name(format!("{name}.corrupt"));
    if !base.exists() {
        return base;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    path.with_file_name(format!("{name}.{nanos}.corrupt"))
}

/// 清理某章节遗留的所有损坏副本（含带时间戳的变体）。
//
// 仅删除“本章节文件名”对应的 `.corrupt` 文件：靠前缀匹配，
// 不会误删别的章节，也不会碰目录里其他文件。
fn remove_corrupt_copies(path: &Path) {
    let Some(dir) = path.parent() else {
        return;
    };
    let Some(name) = path.file_name().map(|s| s.to_string_lossy().to_string()) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name().to_string_lossy().to_string();
        if file_name.starts_with(&format!("{name}.")) && file_name.ends_with(".corrupt") {
            let _ = std::fs::remove_file(entry.path());
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
    fn manifest_is_versioned_and_tracks_chapters() {
        let dir = TempDir::new("manifest");
        let s = storage_in(&dir.0);
        let book = Book {
            title: "书".into(),
            author: "作者".into(),
            cover_url: "u".into(),
            chapters: vec![ChapterEntry {
                id: "ch1".into(),
                title: "第1章".into(),
                url: "a".into(),
                fetch_url: "b".into(),
                update_time: 0,
            }],
        };
        s.save_book("fanqie", "42", &book).expect("保存书籍");
        write_chapter(&s, "正文");
        let path = s.manifest_path("fanqie", "42");
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).expect("读取 manifest"))
                .expect("解析 manifest");
        assert_eq!(manifest["schema_version"], 1);
        assert_eq!(manifest["chapters"][0]["status"], "ok");
        assert_eq!(manifest["chapters"][0]["bytes"], 6);
        assert!(!manifest["chapters"][0]["sha256"]
            .as_str()
            .unwrap_or_default()
            .is_empty());
    }

    #[test]
    fn corrupt_manifest_is_preserved_and_rebuilt() {
        let dir = TempDir::new("manifest-corrupt");
        let s = storage_in(&dir.0);
        let book = Book {
            title: "书".into(),
            author: "作者".into(),
            cover_url: "u".into(),
            chapters: vec![ChapterEntry {
                id: "ch1".into(),
                title: "第1章".into(),
                url: "a".into(),
                fetch_url: "b".into(),
                update_time: 0,
            }],
        };
        s.save_book("fanqie", "42", &book).expect("保存书籍");
        write_chapter(&s, "正文");

        let path = s.manifest_path("fanqie", "42");
        std::fs::write(&path, b"{ not json").expect("写入损坏 manifest");

        assert!(s.repair_manifest("fanqie", "42").expect("修复 manifest"));
        assert!(
            path.with_file_name("fanqie_42.manifest.json.corrupt")
                .exists(),
            "损坏文件必须保留"
        );
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("读取重建后的 manifest"))
                .expect("重建后的 manifest 必须可解析");
        assert_eq!(manifest["chapters"][0]["status"], "ok");
        assert!(!s.repair_manifest("fanqie", "42").expect("再次校验"));
    }

    #[test]
    fn truncated_chapter_is_quarantined_and_redownloadable() {
        let dir = TempDir::new("chapter-corrupt");
        let s = storage_in(&dir.0);
        let path = write_chapter(&s, "一二三四五六七八九十");
        // 模拟写到一半崩溃：文件还在，但内容短了。
        std::fs::write(&path, "一二").expect("截断文件");
        assert!(!s.is_chapter_downloaded("fanqie", "42", "ch1"));

        let backup = path.with_file_name(format!(
            "{}.corrupt",
            path.file_name().expect("文件名").to_string_lossy()
        ));
        assert!(
            backup.exists(),
            "损坏章节必须保留现场: {}",
            backup.display()
        );
        assert!(!path.exists(), "损坏的原文件应已改名");

        // 重新下载成功后不应再留下损坏副本。
        s.save_chapter(
            "fanqie",
            "书",
            "42",
            1,
            "ch1",
            "第1章",
            "一二三四五六七八九十",
        )
        .expect("重新写入");
        assert!(s.is_chapter_downloaded("fanqie", "42", "ch1"));
        assert!(!backup.exists(), "重下成功后应清理损坏副本");
    }

    #[test]
    fn digest_mismatch_is_quarantined() {
        let dir = TempDir::new("digest-corrupt");
        let s = storage_in(&dir.0);
        let path = write_chapter(&s, "正文");
        // 长度不变但内容变了：只有摘要能发现。
        std::fs::write(&path, "伪文").expect("篡改内容");
        assert!(
            !s.is_chapter_downloaded("fanqie", "42", "ch1"),
            "摘要不符必须重新下载"
        );
        assert!(path
            .with_file_name(format!(
                "{}.corrupt",
                path.file_name().expect("文件名").to_string_lossy()
            ))
            .exists());
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
