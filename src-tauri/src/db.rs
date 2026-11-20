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
//! 建表走 [`ensure_schema`]：每个模块把自己的表一次性建好，语句都是
//! `CREATE TABLE IF NOT EXISTS`，重复执行不会破坏已有数据。

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use rusqlite::{Connection, Transaction, TransactionBehavior};

use crate::utils::paths;

const DB_FILE: &str = "minutes.sqlite3";

static SCHEMA_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn schema_lock() -> &'static Mutex<()> {
    SCHEMA_LOCK.get_or_init(|| Mutex::new(()))
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

/// 建表（或确保表存在），在一个 `IMMEDIATE` 事务里完成。
///
/// `create` 只负责写本模块的 DDL。表结构变了就改这里的语句，不做版本迁移——
/// 应用还没分发，改结构时直接删库重建，比维护一条迁移链省事得多。
///
/// 进程内先互斥：历史与笔记的初始化会在不同任务里各自开连接，同时拿写锁会互相
/// 等满 busy_timeout。
pub fn ensure_schema<F>(connection: &Connection, create: F) -> Result<(), String>
where
    F: FnOnce(&Transaction<'_>) -> Result<(), String>,
{
    let _guard = schema_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let transaction = Transaction::new_unchecked(connection, TransactionBehavior::Immediate)
        .map_err(|error| format!("开始建表事务失败: {error}"))?;
    create(&transaction)?;
    transaction
        .commit()
        .map_err(|error| format!("提交建表事务失败: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::OptionalExtension;

    fn memory_connection() -> Connection {
        let connection = Connection::open_in_memory().expect("open in-memory database");
        configure(&connection).expect("configure in-memory database");
        connection
    }

    #[test]
    fn schema_is_created_once_and_repeated_calls_are_safe() {
        let connection = memory_connection();
        for _ in 0..3 {
            ensure_schema(&connection, |transaction| {
                transaction
                    .execute_batch(
                        "CREATE TABLE IF NOT EXISTS alpha_items (id INTEGER PRIMARY KEY);",
                    )
                    .map_err(|error| error.to_string())
            })
            .expect("ensure schema");
        }
        let table: Option<String> = connection
            .query_row(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'alpha_items'",
                [],
                |row| row.get(0),
            )
            .optional()
            .expect("inspect schema");
        assert_eq!(table.as_deref(), Some("alpha_items"));
    }

    #[test]
    fn failed_schema_creation_rolls_back_entirely() {
        let connection = memory_connection();
        let result = ensure_schema(&connection, |transaction| {
            transaction
                .execute_batch("CREATE TABLE alpha_items (id INTEGER PRIMARY KEY);")
                .map_err(|error| error.to_string())?;
            Err("boom".to_string())
        });
        assert!(result.is_err());
        let table: Option<String> = connection
            .query_row(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'alpha_items'",
                [],
                |row| row.get(0),
            )
            .optional()
            .expect("inspect schema");
        assert!(table.is_none(), "失败的建表应整段回滚");
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
