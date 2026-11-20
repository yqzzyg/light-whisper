//! 应用唯一的 SQLite 数据库文件。
//!
//! 历史与笔记同在这个文件里，因为跨模块写必须原子：一次笔记转写要同时写历史行
//! 和笔记 transcript；「历史存为笔记」要读一侧写另一侧。分开存这两处就没有事务
//! 也没有外键，只能靠调用方手工兜底，删笔记后历史行会变成悬空指针。
//!
//! 表名按模块划分：`transcription_history` / `history_audio_leases` 归历史，
//! `notes` / `note_recordings` / `note_folders` 归笔记。代码边界由
//! `history_service` / `notes_service` 承担，互不直接调用对方 SQL。
//!
//! 跨模块约束是 `notes.history_id`、`note_recordings.history_id` 指向
//! `transcription_history(id)`，`ON DELETE SET NULL`：历史被保留期淘汰时只断开引用，
//! 笔记本身不受影响。
//!
//! 版本管理：单文件只有一个 `PRAGMA user_version` 槽，两个模块会互相覆盖，因此各模块的
//! 版本记在 `schema_meta(module, version)`，各自独立推进。

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};

use crate::utils::paths;

const DB_FILE: &str = "minutes.sqlite3";

/// 早期版本的历史库与笔记库文件名。合并成单文件后它们是没人读的孤儿，
/// 启动时删掉，免得一直躺在数据目录里占地方、还让人以为数据在那儿。
const LEGACY_DB_FILES: [&str; 2] = ["transcription_history.sqlite3", "notes.sqlite3"];

static MIGRATION_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn migration_lock() -> &'static Mutex<()> {
    MIGRATION_LOCK.get_or_init(|| Mutex::new(()))
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

/// 打开一个已配置好的连接。
///
/// 各 service 都是「每次操作开一个新连接」的范式，所以这里不返回共享连接，
/// 也就不需要 `Mutex<Connection>` 包裹。
pub fn open() -> Result<Connection, String> {
    let path = paths::get_data_dir().join(DB_FILE);
    if let Some(directory) = path.parent() {
        std::fs::create_dir_all(directory).map_err(|error| format!("创建数据目录失败: {error}"))?;
    }
    let connection = Connection::open(&path).map_err(|error| format!("打开数据库失败: {error}"))?;
    configure(&connection)?;
    Ok(connection)
}

/// 删掉早期版本留下的独立库文件。
///
/// 合并到单文件后这两个文件里的表已经在当前库中，文件本身没人读。删不删都不影响
/// 功能，但留着会让用户以为数据还在那儿，也白占磁盘。删失败只记日志，不挡启动。
pub fn cleanup_legacy_databases() {
    cleanup_legacy_databases_in(&paths::get_data_dir());
}

/// [`cleanup_legacy_databases`] 的可测版本：目录由调用方给，测试用临时目录。
fn cleanup_legacy_databases_in(directory: &std::path::Path) {
    for name in LEGACY_DB_FILES {
        let path = directory.join(name);
        if !path.exists() {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => log::info!("清理旧版数据库文件: {name}"),
            Err(error) => log::warn!("清理旧版数据库文件失败 {}: {error}", path.display()),
        }
    }
}

/// 运行时连接配置，对内存库同样适用，便于测试复用。
///
/// `foreign_keys` 显式打开：本项目 `bundled` 构建的 `SQLITE_DEFAULT_FOREIGN_KEYS` 恰好是 1，
/// 但那是编译期默认值，依赖它会让「换个 sqlite 构建就丢了约束」这种事悄无声息地发生。
pub fn configure(connection: &Connection) -> Result<(), String> {
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(|error| format!("设置数据库等待时间失败: {error}"))?;
    connection
        .execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;",
        )
        .map_err(|error| format!("配置数据库连接失败: {error}"))
}

/// 按模块推进 schema 版本。
///
/// `apply` 收到当前版本，只负责写本模块的表。版本号由这里统一落盘，并且与表结构变更
/// 处于同一个 `IMMEDIATE` 事务里，失败不会留下「表建好了但版本没更新」的半截状态。
pub fn migrate_module<F>(
    connection: &Connection,
    module: &str,
    target: i64,
    apply: F,
) -> Result<(), String>
where
    F: FnOnce(&Transaction<'_>, i64) -> Result<(), String>,
{
    // 两个模块的 initialize 会在不同任务里各自开连接，进程内先互斥，
    // 避免同时拿写锁后互相等满 busy_timeout。
    let _guard = migration_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_meta (
                 module     TEXT PRIMARY KEY,
                 version    INTEGER NOT NULL,
                 updated_at INTEGER NOT NULL
             );",
        )
        .map_err(|error| format!("初始化 schema_meta 失败: {error}"))?;

    let current: i64 = connection
        .query_row(
            "SELECT version FROM schema_meta WHERE module = ?1",
            [module],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| format!("读取 {module} 模块版本失败: {error}"))?
        .unwrap_or(0);

    if current > target {
        return Err(format!(
            "{module} 模块版本 {current} 高于当前支持的 {target}"
        ));
    }
    if current == target {
        return Ok(());
    }

    let transaction = Transaction::new_unchecked(connection, TransactionBehavior::Immediate)
        .map_err(|error| format!("开始 {module} 模块迁移失败: {error}"))?;
    apply(&transaction, current)?;
    transaction
        .execute(
            "INSERT INTO schema_meta (module, version, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(module) DO UPDATE
                 SET version = excluded.version, updated_at = excluded.updated_at",
            params![module, target, now_millis()],
        )
        .map_err(|error| format!("更新 {module} 模块版本失败: {error}"))?;
    transaction
        .commit()
        .map_err(|error| format!("提交 {module} 模块迁移失败: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory_connection() -> Connection {
        let connection = Connection::open_in_memory().expect("open in-memory database");
        configure(&connection).expect("configure in-memory database");
        connection
    }

    /// 造一个一次性临时目录。数据目录是全局 `OnceLock`，测试里改不动，
    /// 所以要测文件清理只能自己给目录。
    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let unique = format!(
            "lw-db-test-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let directory = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&directory).expect("create temp dir");
        directory
    }

    #[test]
    fn legacy_database_files_are_removed_and_current_one_is_kept() {
        let directory = temp_dir("legacy-db-cleanup");
        let current = directory.join(DB_FILE);
        let legacy: Vec<_> = LEGACY_DB_FILES
            .iter()
            .map(|name| directory.join(name))
            .collect();
        for path in legacy.iter().chain(std::iter::once(&current)) {
            std::fs::write(&path, b"placeholder").expect("seed db file");
        }

        cleanup_legacy_databases_in(&directory);

        assert!(current.exists(), "当前库文件不能被删");
        for path in &legacy {
            assert!(!path.exists(), "旧库文件应被删除: {}", path.display());
        }
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn cleanup_is_a_no_op_when_legacy_files_are_absent() {
        let directory = temp_dir("legacy-db-noop");
        // 不该 panic，也不该凭空造出文件。
        cleanup_legacy_databases_in(&directory);
        for name in LEGACY_DB_FILES {
            assert!(!directory.join(name).exists());
        }
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn module_versions_progress_independently() {
        let connection = memory_connection();
        migrate_module(&connection, "alpha", 1, |transaction, current| {
            assert_eq!(current, 0);
            transaction
                .execute_batch("CREATE TABLE alpha_items (id INTEGER PRIMARY KEY);")
                .map_err(|error| error.to_string())
        })
        .expect("migrate alpha to 1");
        migrate_module(&connection, "beta", 1, |transaction, current| {
            assert_eq!(current, 0);
            transaction
                .execute_batch("CREATE TABLE beta_items (id INTEGER PRIMARY KEY);")
                .map_err(|error| error.to_string())
        })
        .expect("migrate beta to 1");
        migrate_module(&connection, "alpha", 2, |transaction, _| {
            transaction
                .execute_batch("ALTER TABLE alpha_items ADD COLUMN note TEXT;")
                .map_err(|error| error.to_string())
        })
        .expect("migrate alpha to 2");

        let alpha: i64 = connection
            .query_row(
                "SELECT version FROM schema_meta WHERE module = 'alpha'",
                [],
                |row| row.get(0),
            )
            .expect("read alpha version");
        let beta: i64 = connection
            .query_row(
                "SELECT version FROM schema_meta WHERE module = 'beta'",
                [],
                |row| row.get(0),
            )
            .expect("read beta version");
        assert_eq!(alpha, 2);
        assert_eq!(beta, 1, "推进 alpha 不应影响 beta");
    }

    #[test]
    fn migration_is_idempotent_and_skips_applied_versions() {
        let connection = memory_connection();
        let mut applied = 0;
        for _ in 0..3 {
            migrate_module(&connection, "alpha", 1, |transaction, _| {
                applied += 1;
                transaction
                    .execute_batch(
                        "CREATE TABLE IF NOT EXISTS alpha_items (id INTEGER PRIMARY KEY);",
                    )
                    .map_err(|error| error.to_string())
            })
            .expect("migrate alpha");
        }
        assert_eq!(applied, 1, "版本到位后不应重复执行迁移体");
    }

    #[test]
    fn failed_migration_leaves_no_version_and_no_table() {
        let connection = memory_connection();
        let result = migrate_module(&connection, "alpha", 1, |transaction, _| {
            transaction
                .execute_batch("CREATE TABLE alpha_items (id INTEGER PRIMARY KEY);")
                .map_err(|error| error.to_string())?;
            Err("boom".to_string())
        });
        assert!(result.is_err());
        let version: Option<i64> = connection
            .query_row(
                "SELECT version FROM schema_meta WHERE module = 'alpha'",
                [],
                |row| row.get(0),
            )
            .optional()
            .expect("read alpha version");
        assert!(version.is_none(), "失败的迁移不应写入版本号");
        let table: Option<String> = connection
            .query_row(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'alpha_items'",
                [],
                |row| row.get(0),
            )
            .optional()
            .expect("inspect schema");
        assert!(table.is_none(), "失败的迁移应整段回滚");
    }

    #[test]
    fn newer_version_than_supported_is_rejected() {
        let connection = memory_connection();
        migrate_module(&connection, "alpha", 2, |_, _| Ok(())).expect("migrate alpha to 2");
        let error = migrate_module(&connection, "alpha", 1, |_, _| Ok(())).expect_err("reject");
        assert!(error.contains("高于当前支持"));
    }

    #[test]
    fn foreign_keys_are_enforced_on_opened_connections() {
        let connection = memory_connection();
        let enabled: i64 = connection
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .expect("read foreign_keys pragma");
        assert_eq!(enabled, 1);
    }
}
