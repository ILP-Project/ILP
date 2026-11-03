//! 旧版数据检测、备份和迁移边界。
//!
//! 默认检测器只识别旧目录的存在，不推断未知数据库的表结构。未来支持某个
//! 格式时可以实现 [`LegacyDetector`] 和 [`LegacyMigrator`]，而无需修改宿主
//! 的备份和事务流程。

use anyhow::{anyhow, Result};
use rusqlite::{params, Connection, OpenFlags, Transaction};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

use crate::{SchemaVersion, StoragePaths};

const SQLITE_HEADER: &[u8] = b"SQLite format 3\0";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum LegacyFormat {
    /// 数据目录不存在可迁移内容。
    Empty,
    /// 已经是当前存储格式。
    Current,
    /// 已发现旧数据，但尚未确认其格式。
    Unknown,
    /// 检测器确认的、可由扩展迁移器处理的格式。
    Known(String),
}

impl LegacyFormat {
    fn needs_migration(&self) -> bool {
        matches!(self, Self::Unknown | Self::Known(_))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum LegacyArtifactKind {
    File,
    Directory,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LegacyArtifact {
    pub path: PathBuf,
    pub kind: LegacyArtifactKind,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LegacyDetectionReport {
    pub data_dir: PathBuf,
    pub format: LegacyFormat,
    pub artifacts: Vec<LegacyArtifact>,
}

impl LegacyDetectionReport {
    pub fn has_legacy_data(&self) -> bool {
        self.format.needs_migration() && !self.artifacts.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackupReport {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub files: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum MigrationStatus {
    NotNeeded,
    Migrated,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MigrationReport {
    pub detection: LegacyDetectionReport,
    pub status: MigrationStatus,
    pub backup: Option<BackupReport>,
    pub records_migrated: u64,
}

#[derive(Debug, Error)]
pub enum MigrationError {
    #[error("旧版数据检测失败: {0}")]
    Detection(String),
    #[error("旧版数据备份失败: {0}")]
    Backup(String),
    #[error("迁移事务失败: {0}")]
    Transaction(String),
    #[error("不支持的旧版存储格式 {format:?}；备份已保存到 {backup:?}")]
    UnsupportedLegacyFormat {
        format: LegacyFormat,
        backup: PathBuf,
        detection: LegacyDetectionReport,
    },
}

/// 只读检测数据目录中的旧版痕迹。
///
/// 检测器不得创建目录、写数据库或改变任何文件。实现可以在未来识别更多
/// 格式，但未知格式必须保留为 [`LegacyFormat::Unknown`]。
pub trait LegacyDetector: Send + Sync {
    fn detect(&self, paths: &StoragePaths) -> Result<LegacyDetectionReport>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultLegacyDetector;

impl LegacyDetector for DefaultLegacyDetector {
    fn detect(&self, paths: &StoragePaths) -> Result<LegacyDetectionReport> {
        paths.validate()?;
        let data_dir = &paths.data_dir;
        if !data_dir.exists() {
            return Ok(LegacyDetectionReport {
                data_dir: data_dir.clone(),
                format: LegacyFormat::Empty,
                artifacts: Vec::new(),
            });
        }
        if !data_dir.is_dir() {
            return Err(anyhow!("数据路径不是目录: {}", data_dir.display()));
        }

        // 这是 ILP-Cpp/早期实现使用过的目录标记。
        let cache_db = data_dir.join("cache.db");
        let novels_dir = data_dir.join("novels");
        if cache_db.exists() || novels_dir.exists() {
            let format = if is_legacy_cache_database(&cache_db) {
                LegacyFormat::Known("cache-v1".into())
            } else {
                LegacyFormat::Unknown
            };
            return Ok(LegacyDetectionReport {
                data_dir: data_dir.clone(),
                format,
                artifacts: collect_entries(data_dir)?,
            });
        }

        if paths.database.exists() {
            let format = if is_current_database(&paths.database) {
                LegacyFormat::Current
            } else {
                LegacyFormat::Unknown
            };
            let artifacts = if matches!(format, LegacyFormat::Unknown) {
                vec![artifact_for(&paths.database)?]
            } else {
                Vec::new()
            };
            return Ok(LegacyDetectionReport {
                data_dir: data_dir.clone(),
                format,
                artifacts,
            });
        }

        Ok(LegacyDetectionReport {
            data_dir: data_dir.clone(),
            format: LegacyFormat::Empty,
            artifacts: Vec::new(),
        })
    }
}

/// 用于将 `cache-v1`（包含 `cache.db` 及 `novels`）迁移至 Core SQLite 架构的迁移器。
#[derive(Debug, Default, Clone, Copy)]
pub struct LegacyCacheMigrator;

impl LegacyMigrator for LegacyCacheMigrator {
    fn format(&self) -> &str {
        "cache-v1"
    }

    fn migrate(
        &self,
        detection: &LegacyDetectionReport,
        transaction: &Transaction<'_>,
    ) -> Result<u64> {
        let cache_db_path = detection.data_dir.join("cache.db");
        if !cache_db_path.exists() {
            return Ok(0);
        }
        let legacy_conn = Connection::open_with_flags(
            &cache_db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )?;

        let mut book_stmt = legacy_conn.prepare(
            "SELECT site_id, book_id, title, author, cover_url, chapters, updated_at FROM books",
        )?;
        let books = book_stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
            ))
        })?;

        let mut count = 0_u64;
        for book in books {
            let (site_id, book_id, title, author, _cover_url, chapters, updated_at) = book?;
            transaction.execute(
                "INSERT INTO books
                 (id, title, author, source_type, source_id, authorized, chapters, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(id) DO UPDATE SET title=excluded.title, author=excluded.author,
                     source_type=excluded.source_type, source_id=excluded.source_id,
                     authorized=excluded.authorized, chapters=excluded.chapters,
                     updated_at=excluded.updated_at",
                params![book_id, title, author, site_id, book_id, true, chapters, updated_at,],
            )?;
            transaction.execute(
                "DELETE FROM search_index WHERE book_id = ?1",
                params![book_id],
            )?;
            transaction.execute(
                "INSERT INTO search_index(book_id, title, author) VALUES (?1, ?2, ?3)",
                params![book_id, title, author],
            )?;
            count += 1;
        }

        let mut chapter_stmt = legacy_conn.prepare(
            "SELECT site_id, book_id, chapter_id, path, status, bytes, sha256 FROM chapters",
        )?;
        let chapters = chapter_stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, String>(6)?,
            ))
        })?;

        for chapter in chapters {
            let (_site_id, book_id, chapter_id, path, status, bytes, sha256) = chapter?;
            if status != "ok" {
                continue;
            }
            transaction.execute(
                "INSERT INTO chapter_files (book_id, chapter_id, path, bytes, sha256)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(book_id, chapter_id) DO UPDATE SET
                     path=excluded.path, bytes=excluded.bytes, sha256=excluded.sha256",
                params![book_id, chapter_id, path, bytes, sha256],
            )?;
        }

        Ok(count)
    }
}

/// 迁移器只负责在宿主提供的 SQLite 事务中转换数据。
///
/// 迁移器不负责备份、提交或回滚；这些动作由 [`crate::SqliteStorage`] 统一
/// 管理。`records_migrated` 会写入迁移报告，便于 CLI 和日志展示。
pub trait LegacyMigrator: Send + Sync {
    fn format(&self) -> &str;
    fn migrate(
        &self,
        detection: &LegacyDetectionReport,
        transaction: &Transaction<'_>,
    ) -> Result<u64>;
}

/// 使用默认的只读检测器，并在无法安全推断 schema 时先备份再返回明确错误。
pub fn detect_legacy_data(paths: &StoragePaths) -> Result<LegacyDetectionReport> {
    DefaultLegacyDetector.detect(paths)
}

/// 将检测到的旧数据复制到新的、不可覆盖的备份目录。
///
/// 备份目标不能位于源目录内，也不能预先存在。符号链接会被拒绝，避免备份
/// 越过数据目录边界。
pub fn backup_legacy_data(
    paths: &StoragePaths,
    detection: &LegacyDetectionReport,
) -> Result<BackupReport> {
    let destination = default_backup_path(&paths.data_dir)?;
    backup_legacy_data_to(paths, detection, destination)
}

pub fn backup_legacy_data_to(
    paths: &StoragePaths,
    detection: &LegacyDetectionReport,
    destination: impl Into<PathBuf>,
) -> Result<BackupReport> {
    let destination = destination.into();
    if detection.data_dir != paths.data_dir {
        return Err(anyhow!("检测报告与数据目录不匹配"));
    }
    if destination == paths.data_dir || destination.starts_with(&paths.data_dir) {
        return Err(anyhow!("备份目录不能位于数据目录内"));
    }
    if destination.exists() {
        return Err(anyhow!(
            "备份目录已存在，不覆盖已有备份: {}",
            destination.display()
        ));
    }

    let mut report = BackupReport {
        source: paths.data_dir.clone(),
        destination: destination.clone(),
        files: 0,
        bytes: 0,
    };
    if let Err(error) = copy_artifacts(paths, detection, &destination, &mut report) {
        let _ = fs::remove_dir_all(&destination);
        return Err(error);
    }
    Ok(report)
}

impl crate::SqliteStorage {
    /// 检测并迁移旧数据。支持安全迁移 `cache-v1` 格式。
    /// 未知 schema 先做完整备份，再返回 [`MigrationError::UnsupportedLegacyFormat`]。
    pub fn migrate_legacy(&mut self) -> std::result::Result<MigrationReport, MigrationError> {
        self.migrate_legacy_with(&DefaultLegacyDetector, Some(&LegacyCacheMigrator))
    }

    /// 只检测旧数据，不创建目录、备份或修改数据库。
    pub fn inspect_legacy(&self) -> std::result::Result<LegacyDetectionReport, MigrationError> {
        DefaultLegacyDetector
            .detect(self.paths())
            .map_err(|error| MigrationError::Detection(error.to_string()))
    }

    /// 使用扩展检测器和迁移器执行安全迁移。备份完成后才会开启 SQLite 事务；
    /// 迁移器失败时事务自动回滚，而备份仍会保留。
    pub fn migrate_legacy_with<D: LegacyDetector>(
        &mut self,
        detector: &D,
        migrator: Option<&dyn LegacyMigrator>,
    ) -> std::result::Result<MigrationReport, MigrationError> {
        let detection = detector
            .detect(self.paths())
            .map_err(|error| MigrationError::Detection(error.to_string()))?;
        if !detection.format.needs_migration() || detection.artifacts.is_empty() {
            return Ok(MigrationReport {
                detection,
                status: MigrationStatus::NotNeeded,
                backup: None,
                records_migrated: 0,
            });
        }

        let backup = backup_legacy_data(self.paths(), &detection)
            .map_err(|error| MigrationError::Backup(error.to_string()))?;
        let Some(migrator) = migrator else {
            return Err(MigrationError::UnsupportedLegacyFormat {
                format: detection.format.clone(),
                backup: backup.destination.clone(),
                detection,
            });
        };
        if !matches!(&detection.format, LegacyFormat::Known(name) if name == migrator.format()) {
            return Err(MigrationError::UnsupportedLegacyFormat {
                format: detection.format.clone(),
                backup: backup.destination.clone(),
                detection,
            });
        }

        let transaction = self
            .db
            .transaction()
            .map_err(|error| MigrationError::Transaction(error.to_string()))?;
        let records_migrated = migrator
            .migrate(&detection, &transaction)
            .map_err(|error| MigrationError::Transaction(error.to_string()))?;
        transaction
            .commit()
            .map_err(|error| MigrationError::Transaction(error.to_string()))?;
        Ok(MigrationReport {
            detection,
            status: MigrationStatus::Migrated,
            backup: Some(backup),
            records_migrated,
        })
    }
}

fn is_current_database(path: &Path) -> bool {
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let mut header = [0_u8; SQLITE_HEADER.len()];
    if file.read_exact(&mut header).is_err() || header != SQLITE_HEADER {
        return false;
    }
    let Ok(connection) = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return false;
    };
    let Ok(value) = connection.query_row(
        "SELECT value FROM schema_meta WHERE key = 'schema_version'",
        [],
        |row| row.get::<_, String>(0),
    ) else {
        return false;
    };
    value
        == format!(
            "{}.{}",
            SchemaVersion::CURRENT.major,
            SchemaVersion::CURRENT.minor
        )
}

fn is_legacy_cache_database(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let mut header = [0_u8; SQLITE_HEADER.len()];
    if file.read_exact(&mut header).is_err() || header != SQLITE_HEADER {
        return false;
    }
    let Ok(connection) = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return false;
    };
    let Ok(mut stmt) = connection.prepare(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name IN ('books', 'chapters')",
    ) else {
        return false;
    };
    let Ok(names) = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .and_then(|rows| rows.collect::<rusqlite::Result<std::collections::HashSet<_>>>())
    else {
        return false;
    };
    names.contains("books") && names.contains("chapters")
}

fn collect_entries(data_dir: &Path) -> Result<Vec<LegacyArtifact>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(data_dir)? {
        let path = entry?.path();
        entries.push(artifact_for(&path)?);
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(entries)
}

fn artifact_for(path: &Path) -> Result<LegacyArtifact> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(anyhow!("不支持备份符号链接: {}", path.display()));
    }
    if metadata.is_dir() {
        let bytes = directory_bytes(path)?;
        Ok(LegacyArtifact {
            path: path.to_path_buf(),
            kind: LegacyArtifactKind::Directory,
            bytes,
        })
    } else if metadata.is_file() {
        Ok(LegacyArtifact {
            path: path.to_path_buf(),
            kind: LegacyArtifactKind::File,
            bytes: metadata.len(),
        })
    } else {
        Err(anyhow!("不支持的旧数据文件类型: {}", path.display()))
    }
}

fn directory_bytes(path: &Path) -> Result<u64> {
    let mut total = 0_u64;
    for entry in fs::read_dir(path)? {
        let child = entry?.path();
        let metadata = fs::symlink_metadata(&child)?;
        if metadata.file_type().is_symlink() {
            return Err(anyhow!("不支持备份符号链接: {}", child.display()));
        }
        total = total.saturating_add(if metadata.is_dir() {
            directory_bytes(&child)?
        } else if metadata.is_file() {
            metadata.len()
        } else {
            return Err(anyhow!("不支持的旧数据文件类型: {}", child.display()));
        });
    }
    Ok(total)
}

fn copy_artifacts(
    paths: &StoragePaths,
    detection: &LegacyDetectionReport,
    destination: &Path,
    report: &mut BackupReport,
) -> Result<()> {
    for artifact in &detection.artifacts {
        let relative = artifact
            .path
            .strip_prefix(&paths.data_dir)
            .map_err(|_| anyhow!("旧数据路径越过数据目录边界: {}", artifact.path.display()))?;
        if relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|component| component == std::path::Component::ParentDir)
        {
            return Err(anyhow!("旧数据相对路径无效: {}", artifact.path.display()));
        }
        let target = destination.join(relative);
        match artifact.kind {
            LegacyArtifactKind::File => copy_file(&artifact.path, &target, report)?,
            LegacyArtifactKind::Directory => copy_directory(&artifact.path, &target, report)?,
        }
    }
    Ok(())
}

fn copy_directory(source: &Path, destination: &Path, report: &mut BackupReport) -> Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        return Err(anyhow!("不支持备份符号链接: {}", source.display()));
    }
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let child = entry?.path();
        let target = destination.join(child.file_name().unwrap_or_default());
        let child_metadata = fs::symlink_metadata(&child)?;
        if child_metadata.file_type().is_symlink() {
            return Err(anyhow!("不支持备份符号链接: {}", child.display()));
        }
        if child_metadata.is_dir() {
            copy_directory(&child, &target, report)?;
        } else if child_metadata.is_file() {
            copy_file(&child, &target, report)?;
        } else {
            return Err(anyhow!("不支持的旧数据文件类型: {}", child.display()));
        }
    }
    Ok(())
}

fn copy_file(source: &Path, destination: &Path, report: &mut BackupReport) -> Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(anyhow!("备份源不是普通文件: {}", source.display()));
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut input = File::open(source)?;
    let mut output = File::create_new(destination)?;
    let copied = std::io::copy(&mut input, &mut output)?;
    output.sync_all()?;
    report.files += 1;
    report.bytes = report.bytes.saturating_add(copied);
    Ok(())
}

fn default_backup_path(data_dir: &Path) -> Result<PathBuf> {
    let parent = data_dir
        .parent()
        .ok_or_else(|| anyhow!("数据目录没有可用的备份父目录"))?;
    let name = data_dir
        .file_name()
        .ok_or_else(|| anyhow!("数据目录没有名称"))?
        .to_string_lossy();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    Ok(parent.join(format!("{name}.backup-{nanos}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LibraryStorage, SqliteStorage};
    use rusqlite::params;

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("ilp-storage-{name}-{}-{nanos}", std::process::id()))
    }

    struct TestDetector {
        report: LegacyDetectionReport,
    }

    impl LegacyDetector for TestDetector {
        fn detect(&self, paths: &StoragePaths) -> Result<LegacyDetectionReport> {
            assert_eq!(&self.report.data_dir, &paths.data_dir);
            Ok(self.report.clone())
        }
    }

    struct TestMigrator {
        fail: bool,
    }

    impl LegacyMigrator for TestMigrator {
        fn format(&self) -> &str {
            "test-legacy"
        }

        fn migrate(
            &self,
            _detection: &LegacyDetectionReport,
            transaction: &Transaction<'_>,
        ) -> Result<u64> {
            transaction.execute(
                "INSERT INTO books
                 (id, title, author, source_type, source_id, authorized, chapters, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    "migrated-book",
                    "迁移测试",
                    "测试作者",
                    "legacy",
                    "legacy-1",
                    false,
                    "[]",
                    0_i64,
                ],
            )?;
            if self.fail {
                return Err(anyhow!("测试迁移失败"));
            }
            Ok(1)
        }
    }

    fn known_detection(data_dir: &Path) -> LegacyDetectionReport {
        LegacyDetectionReport {
            data_dir: data_dir.to_path_buf(),
            format: LegacyFormat::Known("test-legacy".into()),
            artifacts: vec![LegacyArtifact {
                path: data_dir.join("legacy.txt"),
                kind: LegacyArtifactKind::File,
                bytes: 6,
            }],
        }
    }

    #[test]
    fn default_detector_is_read_only_and_does_not_guess_schema() {
        let root = temp_dir("detect");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("novels")).unwrap();
        fs::write(root.join("cache.db"), b"not a known database").unwrap();
        let paths = StoragePaths::new(&root);
        let report = detect_legacy_data(&paths).unwrap();
        assert_eq!(report.format, LegacyFormat::Unknown);
        assert!(report
            .artifacts
            .iter()
            .any(|item| item.path.ends_with("cache.db")));
        assert!(!paths.database.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn backup_copies_legacy_tree_without_overwriting() {
        let root = temp_dir("backup");
        let destination = temp_dir("backup-target");
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&destination);
        fs::create_dir_all(root.join("novels")).unwrap();
        fs::write(root.join("cache.db"), b"cache").unwrap();
        fs::write(root.join("novels/book.txt"), "正文".as_bytes()).unwrap();
        let paths = StoragePaths::new(&root);
        let detection = detect_legacy_data(&paths).unwrap();
        let backup = backup_legacy_data_to(&paths, &detection, &destination).unwrap();
        assert_eq!(backup.files, 2);
        assert_eq!(fs::read(destination.join("cache.db")).unwrap(), b"cache");
        assert_eq!(
            fs::read(destination.join("novels/book.txt")).unwrap(),
            "正文".as_bytes()
        );
        assert!(backup_cannot_overwrite(&paths, &detection, &destination));
        assert_eq!(fs::read(destination.join("cache.db")).unwrap(), b"cache");
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(destination);
    }

    fn backup_cannot_overwrite(
        paths: &StoragePaths,
        detection: &LegacyDetectionReport,
        destination: &Path,
    ) -> bool {
        backup_legacy_data_to(paths, detection, destination).is_err()
    }

    #[test]
    fn migration_api_smoke_returns_structured_report_and_commits() {
        let root = temp_dir("migration-smoke");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("legacy.txt"), b"legacy").unwrap();
        let paths = StoragePaths::new(&root);
        let mut storage = SqliteStorage::open(paths.clone()).unwrap();
        let detector = TestDetector {
            report: known_detection(&root),
        };
        let migrator = TestMigrator { fail: false };
        let report = storage
            .migrate_legacy_with(&detector, Some(&migrator))
            .unwrap();

        assert_eq!(report.status, MigrationStatus::Migrated);
        assert_eq!(report.records_migrated, 1);
        assert_eq!(report.backup.as_ref().unwrap().files, 1);
        assert_eq!(storage.list_book_ids().unwrap(), vec!["migrated-book"]);
        let structured = serde_json::to_value(&report).unwrap();
        assert_eq!(structured["status"], "Migrated");
        assert_eq!(structured["records_migrated"], 1);
        let backup = report.backup.as_ref().unwrap().destination.clone();
        assert!(backup.exists());
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(backup);
    }

    #[test]
    fn failed_migration_rolls_back_database_and_keeps_backup() {
        let root = temp_dir("migration-rollback");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("legacy.txt"), b"legacy").unwrap();
        let paths = StoragePaths::new(&root);
        let mut storage = SqliteStorage::open(paths.clone()).unwrap();
        let backup_prefix = "ilp-storage-migration-rollback-";
        let backups_before = fs::read_dir(root.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(backup_prefix))
            })
            .collect::<std::collections::HashSet<_>>();
        storage
            .save_book(&crate::StoredBook {
                id: "existing-book".into(),
                title: "迁移前的书".into(),
                author: "测试作者".into(),
                source_type: "test".into(),
                source_id: "existing-1".into(),
                authorized: false,
                chapters: Vec::new(),
            })
            .unwrap();
        let detector = TestDetector {
            report: known_detection(&root),
        };
        let migrator = TestMigrator { fail: true };
        let error = storage
            .migrate_legacy_with(&detector, Some(&migrator))
            .unwrap_err();
        assert!(matches!(error, MigrationError::Transaction(_)));
        assert_eq!(storage.list_book_ids().unwrap(), vec!["existing-book"]);
        assert_eq!(fs::read(root.join("legacy.txt")).unwrap(), b"legacy");
        let backups = fs::read_dir(root.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(backup_prefix))
            })
            .filter(|path| !backups_before.contains(path))
            .collect::<Vec<_>>();
        assert_eq!(backups.len(), 1);
        assert_eq!(fs::read(backups[0].join("legacy.txt")).unwrap(), b"legacy");
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(&backups[0]);
    }

    #[test]
    fn unsupported_migration_keeps_backup_and_returns_explicit_error() {
        let root = temp_dir("migrate");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("novels")).unwrap();
        // 写入一个非合法 SQLite 格式或表结构不符的 cache.db，作为未知 schema
        fs::write(root.join("cache.db"), b"corrupted sqlite").unwrap();
        let paths = StoragePaths::new(&root);
        let mut storage = SqliteStorage::open(paths).unwrap();
        let error = storage.migrate_legacy().unwrap_err();
        let MigrationError::UnsupportedLegacyFormat { backup, .. } = error else {
            panic!("expected unsupported format");
        };
        assert!(backup.exists());
        assert!(backup.join("cache.db").exists());
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(backup);
    }

    #[test]
    fn cache_v1_migration_migrates_books_and_chapters_and_creates_backup() {
        let root = temp_dir("cache-v1");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        // 构造旧版 cache.db
        let legacy_db_path = root.join("cache.db");
        let conn = Connection::open(&legacy_db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE books (
                site_id TEXT NOT NULL,
                book_id TEXT NOT NULL,
                title TEXT NOT NULL,
                author TEXT NOT NULL,
                cover_url TEXT NOT NULL,
                chapters TEXT NOT NULL,
                updated_at INTEGER NOT NULL,
                PRIMARY KEY (site_id, book_id)
            );
            CREATE TABLE chapters (
                site_id TEXT NOT NULL,
                book_id TEXT NOT NULL,
                chapter_id TEXT NOT NULL,
                path TEXT NOT NULL,
                status TEXT NOT NULL,
                bytes INTEGER NOT NULL,
                sha256 TEXT NOT NULL,
                mtime INTEGER NOT NULL,
                PRIMARY KEY (site_id, book_id, chapter_id)
            );",
        )
        .unwrap();

        let chapters_json = serde_json::to_string(&vec![ilp_types::ChapterEntry {
            id: "ch-1".into(),
            title: "第一章".into(),
            url: "https://example.com/ch-1".into(),
            fetch_url: "https://example.com/api/ch-1".into(),
            update_time: 100,
            order_key: "001".into(),
        }])
        .unwrap();

        conn.execute(
            "INSERT INTO books VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                "fanqie",
                "12345",
                "旧书名",
                "旧作者",
                "https://example.com/cover.png",
                chapters_json,
                1600000000_i64,
            ],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO chapters VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                "fanqie",
                "12345",
                "ch-1",
                "novels/旧书名 (12345)/0001 - 第一章.txt",
                "ok",
                128_i64,
                "a".repeat(64),
                1600000000_i64,
            ],
        )
        .unwrap();
        drop(conn);

        let paths = StoragePaths::new(&root);
        let mut storage = SqliteStorage::open(paths.clone()).unwrap();

        let detection = storage.inspect_legacy().unwrap();
        assert_eq!(detection.format, LegacyFormat::Known("cache-v1".into()));

        let report = storage.migrate_legacy().unwrap();
        assert_eq!(report.status, MigrationStatus::Migrated);
        assert_eq!(report.records_migrated, 1);
        assert!(report.backup.is_some());

        let book = storage
            .load_book("12345")
            .unwrap()
            .expect("应找到迁移后的书籍");
        assert_eq!(book.title, "旧书名");
        assert_eq!(book.author, "旧作者");
        assert_eq!(book.source_type, "fanqie");
        assert_eq!(book.chapters.len(), 1);

        let chapter_row = storage
            .chapter_file_record("12345", "ch-1")
            .unwrap()
            .expect("应找到迁移后的章节文件记录");
        assert_eq!(chapter_row.0, "novels/旧书名 (12345)/0001 - 第一章.txt");
        assert_eq!(chapter_row.1, 128);
        assert_eq!(chapter_row.2, "a".repeat(64));
        assert_eq!(storage.search("旧书名").unwrap(), vec!["12345"]);

        let backup = report.backup.unwrap().destination;
        assert!(backup.exists());
        assert!(backup.join("cache.db").exists());

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(backup);
    }

    #[test]
    fn cache_v1_migration_skips_failed_and_empty_chapters() {
        let root = temp_dir("cache-v1-partial");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let legacy_db_path = root.join("cache.db");
        let conn = Connection::open(&legacy_db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE books (
                site_id TEXT NOT NULL,
                book_id TEXT NOT NULL,
                title TEXT NOT NULL,
                author TEXT NOT NULL,
                cover_url TEXT NOT NULL,
                chapters TEXT NOT NULL,
                updated_at INTEGER NOT NULL,
                PRIMARY KEY (site_id, book_id)
            );
            CREATE TABLE chapters (
                site_id TEXT NOT NULL,
                book_id TEXT NOT NULL,
                chapter_id TEXT NOT NULL,
                path TEXT NOT NULL,
                status TEXT NOT NULL,
                bytes INTEGER NOT NULL,
                sha256 TEXT NOT NULL,
                mtime INTEGER NOT NULL,
                PRIMARY KEY (site_id, book_id, chapter_id)
            );",
        )
        .unwrap();

        conn.execute(
            "INSERT INTO books VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                "qidian",
                "99999",
                "部分失败的书",
                "作者B",
                "https://example.com/cover2.png",
                "[]",
                1600000000_i64,
            ],
        )
        .unwrap();

        // 插入一条 ok 和一条 failed
        conn.execute(
            "INSERT INTO chapters VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                "qidian",
                "99999",
                "ch-ok",
                "novels/ok.txt",
                "ok",
                256_i64,
                "b".repeat(64),
                1600000000_i64,
            ],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO chapters VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                "qidian",
                "99999",
                "ch-fail",
                "novels/fail.txt",
                "failed",
                0_i64,
                "",
                1600000000_i64,
            ],
        )
        .unwrap();
        drop(conn);

        let paths = StoragePaths::new(&root);
        let mut storage = SqliteStorage::open(paths).unwrap();
        let report = storage.migrate_legacy().unwrap();
        assert_eq!(report.status, MigrationStatus::Migrated);

        assert!(storage
            .chapter_file_record("99999", "ch-ok")
            .unwrap()
            .is_some());
        assert!(storage
            .chapter_file_record("99999", "ch-fail")
            .unwrap()
            .is_none());

        let backup = report.backup.unwrap().destination;
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(backup);
    }
}
