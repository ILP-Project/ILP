//! 旧版数据检测、备份和迁移边界。
//!
//! 默认检测器只识别旧目录的存在，不推断未知数据库的表结构。未来支持某个
//! 格式时可以实现 [`LegacyDetector`] 和 [`LegacyMigrator`]，而无需修改宿主
//! 的备份和事务流程。

use anyhow::{anyhow, Result};
use rusqlite::{Connection, OpenFlags, Transaction};
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

        // 这是 ILP-Cpp/早期实现使用过的目录标记。这里只据此报告“未知”，
        // 不读取 cache.db 的表名，也不把目录内容解释成新 schema。
        let old_markers = [data_dir.join("cache.db"), data_dir.join("novels")];
        let has_old_marker = old_markers.iter().any(|path| path.exists());
        if has_old_marker {
            return Ok(LegacyDetectionReport {
                data_dir: data_dir.clone(),
                format: LegacyFormat::Unknown,
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
    /// 检测并迁移旧数据。默认流程不猜测未知 schema，因此会先做完整备份，
    /// 再返回 [`MigrationError::UnsupportedLegacyFormat`]。
    pub fn migrate_legacy(&mut self) -> std::result::Result<MigrationReport, MigrationError> {
        self.migrate_legacy_with(&DefaultLegacyDetector, None)
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
    use crate::SqliteStorage;

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("ilp-storage-{name}-{}", std::process::id()))
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
    fn unsupported_migration_keeps_backup_and_returns_explicit_error() {
        let root = temp_dir("migrate");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("novels")).unwrap();
        fs::write(root.join("cache.db"), b"legacy").unwrap();
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
}
