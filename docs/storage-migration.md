# 数据目录迁移

`ilp-storage` 提供只读旧数据检测、不可覆盖备份和事务迁移入口。默认检测器不会猜测旧数据库的表结构或字段含义。

## 检测

`detect_legacy_data(&StoragePaths)` 不创建目录、不打开可写数据库，也不修改文件。它会报告：

- `Empty`：没有检测到数据；
- `Current`：数据库包含当前 schema 标记；
- `Unknown`：发现旧目录标记（例如 `cache.db` 或 `novels`），但格式未被确认；
- `Known(name)`：由扩展检测器确认、可交给同名迁移器处理的格式。

未知数据会保留 `LegacyDetectionReport` 和文件清单，不对旧内容作解释。检测器实现 `LegacyDetector` 即可增加格式识别。

## 备份

`backup_legacy_data` 会把检测报告中的完整数据树复制到同级的新目录。备份目标不能位于源目录内，不能覆盖已有目录，符号链接会被拒绝。备份失败会清理本次新建的目标目录。

## 迁移

```rust,no_run
use ilp_storage::{MigrationError, SqliteStorage, StoragePaths};

let mut storage = SqliteStorage::open(StoragePaths::new("data"))?;
match storage.migrate_legacy() {
    Ok(report) => println!("迁移状态: {:?}", report.status),
    Err(MigrationError::UnsupportedLegacyFormat { backup, .. }) => {
        eprintln!("旧格式尚未支持，备份位于 {}", backup.display());
    }
    Err(error) => return Err(error),
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

默认入口先备份，再返回 `UnsupportedLegacyFormat`；不会删除或改写旧数据。支持某个格式时，实现 `LegacyMigrator`，并通过 `migrate_legacy_with` 注册检测器和迁移器。迁移器收到宿主开启的 SQLite 事务，只返回迁移记录数；成功才提交，失败自动回滚，备份始终保留。
