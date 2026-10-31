//! ILP 核心持久化边界。
//!
//! 该 crate 只负责稳定的数据存储协议和迁移入口；上层通过此边界访问
//! SQLite、正文文件和搜索索引，不应直接依赖具体数据库实现。

use anyhow::{anyhow, Result};

mod migration;
use ilp_types::{ChapterEntry, PluginMetadata};
pub use migration::{
    backup_legacy_data, backup_legacy_data_to, detect_legacy_data, BackupReport,
    DefaultLegacyDetector, LegacyArtifact, LegacyArtifactKind, LegacyDetectionReport,
    LegacyDetector, LegacyFormat, LegacyMigrator, MigrationError, MigrationReport, MigrationStatus,
};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoragePaths {
    pub data_dir: PathBuf,
    pub database: PathBuf,
    pub content_dir: PathBuf,
    pub covers_dir: PathBuf,
    pub plugins_dir: PathBuf,
}

impl StoragePaths {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        let data_dir = data_dir.into();
        Self {
            database: data_dir.join("library.db"),
            content_dir: data_dir.join("content"),
            covers_dir: data_dir.join("covers"),
            plugins_dir: data_dir.join("plugins"),
            data_dir,
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.data_dir.as_os_str().is_empty()
            || self.data_dir.is_absolute() && self.data_dir == Path::new("/")
        {
            return Err(anyhow!("数据目录无效"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct SchemaVersion {
    pub major: u32,
    pub minor: u32,
}

impl SchemaVersion {
    pub const CURRENT: Self = Self { major: 1, minor: 0 };
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredBook {
    pub id: String,
    pub title: String,
    pub author: String,
    pub source_type: String,
    pub source_id: String,
    pub authorized: bool,
    pub chapters: Vec<ChapterEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredPlugin {
    pub metadata: PluginMetadata,
    pub content_hash: String,
    pub signature_key_id: Option<String>,
    pub source: String,
    pub trusted_at: Option<String>,
}

pub trait LibraryStorage: Send {
    fn schema_version(&self) -> Result<SchemaVersion>;
    fn save_book(&mut self, book: &StoredBook) -> Result<()>;
    fn load_book(&mut self, id: &str) -> Result<Option<StoredBook>>;
}

/// SQLite-backed storage used by Core. Files are written atomically and their
/// digest is recorded alongside the metadata so interrupted writes are never
/// mistaken for completed chapters.
pub struct SqliteStorage {
    db: Connection,
    paths: StoragePaths,
}

impl SqliteStorage {
    pub fn open(paths: StoragePaths) -> Result<Self> {
        paths.validate()?;
        std::fs::create_dir_all(&paths.data_dir)?;
        std::fs::create_dir_all(&paths.content_dir)?;
        std::fs::create_dir_all(&paths.covers_dir)?;
        std::fs::create_dir_all(&paths.plugins_dir)?;
        let db = Connection::open(&paths.database)?;
        migrate(&db)?;
        Ok(Self { db, paths })
    }

    pub fn create_task(&self, id: &str, kind: &str, state: &str) -> Result<()> {
        let now = unix_seconds()?;
        self.db.execute(
            "INSERT INTO tasks (id, kind, state, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?4)",
            params![id, kind, state, now],
        )?;
        Ok(())
    }

    pub fn set_task_state(&self, id: &str, state: &str) -> Result<()> {
        self.db.execute(
            "UPDATE tasks SET state = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, state, unix_seconds()?],
        )?;
        Ok(())
    }

    pub fn create_task_with_event<F>(
        &self,
        id: &str,
        kind: &str,
        state: &str,
        event_kind: &str,
        make_payload: F,
    ) -> Result<u64>
    where
        F: FnOnce(u64) -> Result<String>,
    {
        let tx = self.db.unchecked_transaction()?;
        let now = unix_seconds()?;
        tx.execute(
            "INSERT INTO tasks (id, kind, state, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?4)",
            params![id, kind, state, now],
        )?;
        let sequence = next_event_sequence_tx(&tx)?;
        let payload = make_payload(sequence)?;
        tx.execute(
            "INSERT INTO events (sequence, kind, payload, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![sequence as i64, event_kind, payload, now],
        )?;
        tx.commit()?;
        Ok(sequence)
    }

    pub fn set_task_state_with_event<F>(
        &self,
        id: &str,
        state: &str,
        event_kind: &str,
        make_payload: F,
    ) -> Result<u64>
    where
        F: FnOnce(u64) -> Result<String>,
    {
        let tx = self.db.unchecked_transaction()?;
        let now = unix_seconds()?;
        let changed = tx.execute(
            "UPDATE tasks SET state = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, state, now],
        )?;
        if changed != 1 {
            return Err(anyhow!("任务不存在: {id}"));
        }
        let sequence = next_event_sequence_tx(&tx)?;
        let payload = make_payload(sequence)?;
        tx.execute(
            "INSERT INTO events (sequence, kind, payload, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![sequence as i64, event_kind, payload, now],
        )?;
        tx.commit()?;
        Ok(sequence)
    }

    pub fn recover_interrupted_tasks_with_events<F>(
        &self,
        mut make_payload: F,
    ) -> Result<Vec<String>>
    where
        F: FnMut(&str, u64) -> Result<String>,
    {
        let tx = self.db.unchecked_transaction()?;
        let mut stmt =
            tx.prepare("SELECT id FROM tasks WHERE state = 'running' ORDER BY created_at")?;
        let task_ids = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        let now = unix_seconds()?;
        let mut recovered = Vec::with_capacity(task_ids.len());
        for id in task_ids {
            tx.execute(
                "UPDATE tasks SET state = 'interrupted', updated_at = ?2 WHERE id = ?1",
                params![id, now],
            )?;
            let sequence = next_event_sequence_tx(&tx)?;
            let payload = make_payload(&id, sequence)?;
            tx.execute(
                "INSERT INTO events (sequence, kind, payload, created_at) VALUES (?1, ?2, ?3, ?4)",
                params![sequence as i64, "task_state_changed", payload, now],
            )?;
            recovered.push(id);
        }
        tx.commit()?;
        Ok(recovered)
    }

    pub fn task_state(&self, id: &str) -> Result<Option<String>> {
        self.db
            .query_row("SELECT state FROM tasks WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .optional()
            .map_err(Into::into)
    }

    pub fn task_states(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self
            .db
            .prepare("SELECT id, state FROM tasks ORDER BY created_at")?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn append_event(&self, kind: &str, payload: &str) -> Result<u64> {
        self.append_event_with_payload(kind, |_| Ok(payload.to_owned()))
    }

    pub fn append_event_with_payload<F>(&self, kind: &str, make_payload: F) -> Result<u64>
    where
        F: FnOnce(u64) -> Result<String>,
    {
        let tx = self.db.unchecked_transaction()?;
        let sequence = next_event_sequence_tx(&tx)?;
        let payload = make_payload(sequence)?;
        tx.execute(
            "INSERT INTO events (sequence, kind, payload, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![sequence as i64, kind, payload, unix_seconds()?],
        )?;
        tx.commit()?;
        Ok(sequence)
    }

    pub fn events_after(&self, sequence: u64, limit: u32) -> Result<Vec<(u64, String, String)>> {
        let mut stmt = self.db.prepare(
            "SELECT sequence, kind, payload FROM events WHERE sequence > ?1 ORDER BY sequence LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![sequence as i64, limit], |row| {
            Ok((row.get::<_, i64>(0)? as u64, row.get(1)?, row.get(2)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn oldest_event_sequence(&self) -> Result<Option<u64>> {
        self.db
            .query_row("SELECT MIN(sequence) FROM events", [], |row| {
                row.get::<_, Option<i64>>(0)
            })
            .map(|value| value.map(|sequence| sequence as u64))
            .map_err(Into::into)
    }

    pub fn current_event_sequence(&self) -> Result<u64> {
        self.db
            .query_row(
                "SELECT COALESCE((SELECT next_sequence FROM event_sequence WHERE id = 1), 0)",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map(|sequence| sequence as u64)
            .map_err(Into::into)
    }

    pub fn prune_events(&self, keep: u64) -> Result<usize> {
        let removed = self.db.execute(
            "DELETE FROM events WHERE sequence <= (SELECT COALESCE(MAX(sequence), 0) - ?1 FROM events)",
            [keep as i64],
        )?;
        Ok(removed)
    }

    pub fn next_event_sequence(&self) -> Result<u64> {
        let tx = self.db.unchecked_transaction()?;
        let value = next_event_sequence_tx(&tx)?;
        tx.commit()?;
        Ok(value)
    }

    pub fn paths(&self) -> &StoragePaths {
        &self.paths
    }

    pub fn index_book(&self, book_id: &str, title: &str, author: &str) -> Result<()> {
        self.db
            .execute("DELETE FROM search_index WHERE book_id = ?1", [book_id])?;
        self.db.execute(
            "INSERT INTO search_index(book_id, title, author) VALUES (?1, ?2, ?3)",
            params![book_id, title, author],
        )?;
        Ok(())
    }

    pub fn index_chapter(
        &self,
        book_id: &str,
        chapter_id: &str,
        title: &str,
        body: &str,
    ) -> Result<()> {
        self.db.execute(
            "DELETE FROM search_index WHERE book_id = ?1 AND chapter_title = ?2",
            params![book_id, title],
        )?;
        self.db.execute(
            "INSERT INTO search_index(book_id, chapter_title, body) VALUES (?1, ?2, ?3)",
            params![book_id, title, body],
        )?;
        let _ = chapter_id;
        Ok(())
    }

    pub fn save_chapter(&self, book_id: &str, chapter_id: &str, content: &[u8]) -> Result<PathBuf> {
        let safe_book = sanitize_component(book_id);
        let safe_chapter = sanitize_component(chapter_id);
        let dir = self.paths.content_dir.join(&safe_book);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{safe_chapter}.txt"));
        atomic_write(&path, content)?;
        let digest = format!("{:x}", Sha256::digest(content));
        self.db.execute(
            "INSERT INTO chapter_files (book_id, chapter_id, path, bytes, sha256)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(book_id, chapter_id) DO UPDATE SET
                 path=excluded.path, bytes=excluded.bytes, sha256=excluded.sha256",
            params![
                book_id,
                chapter_id,
                path.to_string_lossy(),
                content.len() as i64,
                digest
            ],
        )?;
        Ok(path)
    }

    pub fn read_chapter(&self, book_id: &str, chapter_id: &str) -> Result<Option<Vec<u8>>> {
        let row = self.db.query_row(
            "SELECT path, bytes, sha256 FROM chapter_files WHERE book_id = ?1 AND chapter_id = ?2",
            params![book_id, chapter_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64, row.get::<_, String>(2)?)),
        ).optional()?;
        let Some((path, bytes, expected)) = row else {
            return Ok(None);
        };
        let data = std::fs::read(&path)?;
        if data.len() as u64 != bytes || format!("{:x}", Sha256::digest(&data)) != expected {
            return Err(anyhow!("章节文件校验失败: {path}"));
        }
        Ok(Some(data))
    }

    pub fn rebuild_search_index(&self) -> Result<()> {
        self.db.execute_batch(
            "DROP TABLE IF EXISTS search_index;
             CREATE VIRTUAL TABLE search_index USING fts5(
                 book_id UNINDEXED, title, author, chapter_title, body
             );
             INSERT INTO search_index(book_id, title, author)
                 SELECT id, title, author FROM books;",
        )?;
        Ok(())
    }

    pub fn list_book_ids(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .db
            .prepare("SELECT id FROM books ORDER BY title COLLATE NOCASE")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn search(&self, query: &str) -> Result<Vec<String>> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let mut stmt = self.db.prepare(
            "SELECT book_id FROM search_index WHERE search_index MATCH ?1 ORDER BY rank",
        )?;
        let results = stmt
            .query_map([query], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if !results.is_empty() {
            return Ok(results);
        }
        // SQLite 的默认 FTS tokenizer 对部分 CJK 文本不会产生可检索 token；
        // 使用参数化 LIKE 作为派生索引的安全兜底，不拼接用户输入到 SQL。
        let pattern = format!("%{query}%");
        let mut fallback = self.db.prepare(
            "SELECT DISTINCT book_id FROM search_index
             WHERE title LIKE ?1 OR author LIKE ?1 OR chapter_title LIKE ?1 OR body LIKE ?1",
        )?;
        let rows = fallback.query_map([pattern], |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn schema_version(&self) -> Result<SchemaVersion> {
        let value: String = self.db.query_row(
            "SELECT value FROM schema_meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )?;
        let (major, minor) = value
            .split_once('.')
            .ok_or_else(|| anyhow!("schema 版本无效"))?;
        Ok(SchemaVersion {
            major: major.parse()?,
            minor: minor.parse()?,
        })
    }
}

fn migrate(db: &Connection) -> Result<()> {
    let transaction = db.unchecked_transaction()?;
    transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_meta (
             key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS books (
             id TEXT PRIMARY KEY NOT NULL, title TEXT NOT NULL, author TEXT NOT NULL,
             source_type TEXT NOT NULL, source_id TEXT NOT NULL, authorized INTEGER NOT NULL,
             chapters TEXT NOT NULL, updated_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS chapter_files (
             book_id TEXT NOT NULL, chapter_id TEXT NOT NULL, path TEXT NOT NULL,
             bytes INTEGER NOT NULL, sha256 TEXT NOT NULL,
             PRIMARY KEY (book_id, chapter_id)
         );
         CREATE TABLE IF NOT EXISTS tasks (
             id TEXT PRIMARY KEY NOT NULL,
             kind TEXT NOT NULL,
             state TEXT NOT NULL,
             created_at INTEGER NOT NULL,
             updated_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS event_sequence (
             id INTEGER PRIMARY KEY CHECK (id = 1),
             next_sequence INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS events (
             sequence INTEGER PRIMARY KEY NOT NULL,
             kind TEXT NOT NULL,
             payload TEXT NOT NULL,
             created_at INTEGER NOT NULL
         );
         CREATE VIRTUAL TABLE IF NOT EXISTS search_index USING fts5(
             book_id UNINDEXED, title, author, chapter_title, body
         );
         CREATE TABLE IF NOT EXISTS events (
             sequence INTEGER PRIMARY KEY NOT NULL, kind TEXT NOT NULL,
             payload TEXT NOT NULL, created_at INTEGER NOT NULL
         );",
    )?;
    transaction.execute(
        "INSERT INTO schema_meta (key, value) VALUES ('schema_version', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [format!(
            "{}.{}",
            SchemaVersion::CURRENT.major,
            SchemaVersion::CURRENT.minor
        )],
    )?;
    transaction.commit()?;
    Ok(())
}

fn sanitize_component(value: &str) -> String {
    let value: String = value
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '*') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let trimmed = value.trim_matches(['.', ' ']);
    if trimmed.is_empty() {
        "untitled".into()
    } else {
        trimmed.into()
    }
}

impl LibraryStorage for SqliteStorage {
    fn schema_version(&self) -> Result<SchemaVersion> {
        SqliteStorage::schema_version(self)
    }

    fn save_book(&mut self, book: &StoredBook) -> Result<()> {
        let chapters = serde_json::to_string(&book.chapters)?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
        self.db.execute(
            "INSERT INTO books (id, title, author, source_type, source_id, authorized, chapters, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET title=excluded.title, author=excluded.author,
                 source_type=excluded.source_type, source_id=excluded.source_id,
                 authorized=excluded.authorized, chapters=excluded.chapters,
                 updated_at=excluded.updated_at",
            params![
                book.id,
                book.title,
                book.author,
                book.source_type,
                book.source_id,
                book.authorized,
                chapters,
                now,
            ],
        )?;
        Ok(())
    }

    fn load_book(&mut self, id: &str) -> Result<Option<StoredBook>> {
        self.db
            .query_row(
                "SELECT id, title, author, source_type, source_id, authorized, chapters
             FROM books WHERE id = ?1",
                [id],
                |row| {
                    let chapters: String = row.get(6)?;
                    Ok(StoredBook {
                        id: row.get(0)?,
                        title: row.get(1)?,
                        author: row.get(2)?,
                        source_type: row.get(3)?,
                        source_id: row.get(4)?,
                        authorized: row.get::<_, i64>(5)? != 0,
                        chapters: serde_json::from_str(&chapters).map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                6,
                                rusqlite::types::Type::Text,
                                Box::new(error),
                            )
                        })?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }
}

fn next_event_sequence_tx(tx: &Transaction<'_>) -> Result<u64> {
    tx.execute(
        "INSERT INTO event_sequence (id, next_sequence) VALUES (1, 1)
         ON CONFLICT(id) DO UPDATE SET next_sequence = next_sequence + 1",
        [],
    )?;
    let value: i64 = tx.query_row(
        "SELECT next_sequence FROM event_sequence WHERE id = 1",
        [],
        |row| row.get(0),
    )?;
    Ok(value as u64)
}

fn unix_seconds() -> Result<i64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64)
}

fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| anyhow!("文件路径没有父目录"))?;
    std::fs::create_dir_all(parent)?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let tmp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        nanos
    ));
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(data)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_paths_are_deterministic() {
        let paths = StoragePaths::new("data");
        assert_eq!(paths.database, PathBuf::from("data/library.db"));
        assert_eq!(paths.content_dir, PathBuf::from("data/content"));
    }

    #[test]
    fn current_schema_is_explicit() {
        assert_eq!(SchemaVersion::CURRENT, SchemaVersion { major: 1, minor: 0 });
    }

    #[test]
    fn sqlite_storage_round_trips_chapter_and_search() {
        let root = std::env::temp_dir().join(format!("ilp-storage-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let storage = SqliteStorage::open(StoragePaths::new(&root)).unwrap();
        storage
            .save_chapter("book/1", "chapter/1", "正文内容".as_bytes())
            .unwrap();
        assert_eq!(
            storage.read_chapter("book/1", "chapter/1").unwrap(),
            Some("正文内容".as_bytes().to_vec())
        );
        storage.index_book("book/1", "测试书", "测试作者").unwrap();
        storage
            .index_chapter("book/1", "chapter/1", "第一章", "正文内容")
            .unwrap();
        assert_eq!(storage.search("正文").unwrap(), vec!["book/1"]);
        assert_eq!(storage.schema_version().unwrap(), SchemaVersion::CURRENT);
        let _ = std::fs::remove_dir_all(root);
    }
}
