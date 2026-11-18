//! 笔记与笔记目录的持久化。
//!
//! 与历史库完全独立：独立的 `notes.sqlite3`、独立的 schema 版本、独立的 `note_folders` /
//! `notes` 两张表。历史库那边有 GC、租约、重识别等强语义，笔记只是「目录 + 一份文档 + 转录」，
//! 混进同一张表会把两套生命周期绑在一起。
//!
//! 约定：
//!
//! - 目录只有一层，`notes.folder_id` 为 `NULL` 表示「未分类」。
//! - 删除目录不删笔记，只把目录内笔记的 `folder_id` 置空，且与删目录在同一个写事务里完成。
//! - `title` 允许为空，展示层兜底成「未命名笔记」，不在库里写死语言相关文案。
//! - 文本内容只在这里落库；音频文件不归本模块管（见 `note_recording_service`），本模块只存文件名。

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::types::Value;
use rusqlite::{
    params, params_from_iter, Connection, OptionalExtension, Row, Transaction, TransactionBehavior,
};
use serde::{Deserialize, Serialize};
use tokio::sync::OnceCell;

use crate::utils::paths;

const NOTES_DB_FILE: &str = "notes.sqlite3";
const NOTES_SCHEMA_VERSION: i64 = 1;
const DEFAULT_PAGE_SIZE: u32 = 50;
const MAX_PAGE_SIZE: u32 = 200;
/// 合法的笔记类型，与前端 `NoteKind` 一致。
const NOTE_KINDS: [&str; 3] = ["personal", "meeting", "import"];

static NOTES_INITIALIZED: OnceCell<()> = OnceCell::const_new();

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn notes_db_path() -> PathBuf {
    paths::get_data_dir().join(NOTES_DB_FILE)
}

fn open_connection() -> Result<Connection, String> {
    std::fs::create_dir_all(paths::get_data_dir())
        .map_err(|error| format!("创建笔记数据目录失败: {error}"))?;
    let connection = Connection::open(notes_db_path())
        .map_err(|error| format!("打开笔记数据库失败: {error}"))?;
    configure_runtime_connection(&connection)?;
    Ok(connection)
}

fn configure_runtime_connection(connection: &Connection) -> Result<(), String> {
    connection
        .busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|error| format!("设置笔记数据库等待时间失败: {error}"))?;
    connection
        .execute_batch("PRAGMA synchronous = NORMAL;")
        .map_err(|error| format!("配置笔记数据库失败: {error}"))
}

fn migrate_schema(connection: &Connection) -> Result<(), String> {
    connection
        .execute_batch("PRAGMA journal_mode = WAL;")
        .map_err(|error| format!("启用笔记数据库 WAL 失败: {error}"))?;
    let current_version = connection
        .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
        .map_err(|error| format!("读取笔记数据库版本失败: {error}"))?;
    if current_version > NOTES_SCHEMA_VERSION {
        return Err(format!(
            "笔记数据库版本 {current_version} 高于当前支持的 {NOTES_SCHEMA_VERSION}"
        ));
    }
    if current_version == NOTES_SCHEMA_VERSION {
        return Ok(());
    }

    let transaction = Transaction::new_unchecked(connection, TransactionBehavior::Immediate)
        .map_err(|error| format!("开始笔记数据库迁移失败: {error}"))?;
    transaction
        .execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS note_folders (
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                name       TEXT    NOT NULL,
                sort_order INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE UNIQUE INDEX IF NOT EXISTS idx_note_folders_name ON note_folders(name);
            CREATE TABLE IF NOT EXISTS notes (
                id                   INTEGER PRIMARY KEY AUTOINCREMENT,
                folder_id            INTEGER REFERENCES note_folders(id) ON DELETE SET NULL,
                title                TEXT    NOT NULL DEFAULT '',
                content              TEXT    NOT NULL DEFAULT '',
                transcript           TEXT,
                summary              TEXT,
                summary_model        TEXT,
                summary_prompt       TEXT,
                summary_generated_at INTEGER,
                summary_source_hash  TEXT,
                kind                 TEXT    NOT NULL DEFAULT 'personal',
                duration_sec         REAL,
                history_id           INTEGER,
                audio_file           TEXT,
                created_at           INTEGER NOT NULL,
                updated_at           INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_notes_folder  ON notes(folder_id, updated_at DESC);
            CREATE INDEX IF NOT EXISTS idx_notes_updated ON notes(updated_at DESC);
            "#,
        )
        .map_err(|error| format!("初始化笔记数据库失败: {error}"))?;
    transaction
        .pragma_update(None, "user_version", NOTES_SCHEMA_VERSION)
        .map_err(|error| format!("更新笔记数据库版本失败: {error}"))?;
    transaction
        .commit()
        .map_err(|error| format!("提交笔记数据库迁移失败: {error}"))
}

/// 幂等初始化。所有对外函数都先调它，与历史库保持同一范式。
pub async fn initialize() -> Result<(), String> {
    NOTES_INITIALIZED
        .get_or_try_init(|| async {
            tokio::task::spawn_blocking(|| {
                let connection = open_connection()?;
                migrate_schema(&connection)
            })
            .await
            .map_err(|error| format!("初始化笔记数据库任务失败: {error}"))?
        })
        .await
        .map(|_| ())
}

// ---------------------------------------------------------------------------
// 数据结构
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NoteFolder {
    pub id: i64,
    pub name: String,
    pub sort_order: i64,
    pub note_count: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NoteListItem {
    pub id: i64,
    pub folder_id: Option<i64>,
    pub title: String,
    pub kind: String,
    pub excerpt: String,
    pub duration_sec: Option<f64>,
    pub has_transcript: bool,
    pub has_summary: bool,
    pub has_audio: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NoteDetail {
    pub id: i64,
    pub folder_id: Option<i64>,
    pub title: String,
    pub content: String,
    pub transcript: Option<String>,
    pub summary: Option<String>,
    pub summary_model: Option<String>,
    pub summary_generated_at: Option<i64>,
    pub summary_source_hash: Option<String>,
    pub kind: String,
    pub duration_sec: Option<f64>,
    pub history_id: Option<i64>,
    pub audio_file: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotePage {
    pub items: Vec<NoteListItem>,
    pub total: u64,
    pub has_more: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NoteQuery {
    #[serde(default)]
    pub folder_id: Option<i64>,
    /// 只看「未分类」。与 `folder_id` 同时给出时以本项为准。
    #[serde(default)]
    pub unfiled: bool,
    #[serde(default)]
    pub query: String,
    #[serde(default = "default_limit")]
    pub limit: u32,
    #[serde(default)]
    pub offset: u32,
}

fn default_limit() -> u32 {
    DEFAULT_PAGE_SIZE
}

/// 局部更新：只写传进来的字段。
///
/// 目录归属走独立的 `move_note`，所以这里不需要「传 null 表示清空」的双层 Option，
/// 参数含义保持单一，避免前端拼出 `{folderId: null}` 时被误解成「不改」。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NotePatch {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
}

// ---------------------------------------------------------------------------
// 目录
// ---------------------------------------------------------------------------

fn folder_from_row(row: &Row<'_>) -> rusqlite::Result<NoteFolder> {
    Ok(NoteFolder {
        id: row.get(0)?,
        name: row.get(1)?,
        sort_order: row.get(2)?,
        note_count: row.get(3)?,
        created_at: row.get(4)?,
        updated_at: row.get(5)?,
    })
}

fn list_folders_with_connection(connection: &Connection) -> Result<Vec<NoteFolder>, String> {
    let mut statement = connection
        .prepare(
            "SELECT f.id, f.name, f.sort_order, \
                    (SELECT COUNT(*) FROM notes n WHERE n.folder_id = f.id), \
                    f.created_at, f.updated_at \
             FROM note_folders f ORDER BY f.sort_order ASC, f.id ASC",
        )
        .map_err(|error| format!("准备查询笔记目录失败: {error}"))?;
    let rows = statement
        .query_map([], folder_from_row)
        .map_err(|error| format!("查询笔记目录失败: {error}"))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| format!("解析笔记目录失败: {error}"))
}

fn normalize_folder_name(name: &str) -> Result<String, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("目录名不能为空".into());
    }
    if trimmed.chars().count() > 60 {
        return Err("目录名不能超过 60 个字符".into());
    }
    Ok(trimmed.to_string())
}

fn ensure_folder_name_available(
    connection: &Connection,
    name: &str,
    exclude_id: Option<i64>,
) -> Result<(), String> {
    let existing: Option<i64> = connection
        .query_row(
            "SELECT id FROM note_folders WHERE name = ?1 AND (?2 IS NULL OR id <> ?2)",
            params![name, exclude_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| format!("检查目录重名失败: {error}"))?;
    if existing.is_some() {
        return Err(format!("已存在同名目录「{name}」"));
    }
    Ok(())
}

fn insert_folder_with_connection(
    connection: &Connection,
    name: &str,
) -> Result<NoteFolder, String> {
    let name = normalize_folder_name(name)?;
    ensure_folder_name_available(connection, &name, None)?;
    let now = now_millis();
    let next_order: i64 = connection
        .query_row(
            "SELECT COALESCE(MAX(sort_order), -1) + 1 FROM note_folders",
            [],
            |row| row.get(0),
        )
        .map_err(|error| format!("读取目录排序失败: {error}"))?;
    connection
        .execute(
            "INSERT INTO note_folders (name, sort_order, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?3)",
            params![name, next_order, now],
        )
        .map_err(|error| format!("创建目录失败: {error}"))?;
    Ok(NoteFolder {
        id: connection.last_insert_rowid(),
        name,
        sort_order: next_order,
        note_count: 0,
        created_at: now,
        updated_at: now,
    })
}

fn rename_folder_with_connection(
    connection: &Connection,
    id: i64,
    name: &str,
) -> Result<NoteFolder, String> {
    let name = normalize_folder_name(name)?;
    ensure_folder_name_available(connection, &name, Some(id))?;
    let changed = connection
        .execute(
            "UPDATE note_folders SET name = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, name, now_millis()],
        )
        .map_err(|error| format!("重命名目录失败: {error}"))?;
    if changed == 0 {
        return Err("目录不存在".into());
    }
    list_folders_with_connection(connection)?
        .into_iter()
        .find(|folder| folder.id == id)
        .ok_or_else(|| "目录不存在".to_string())
}

/// 删目录：目录内笔记的 `folder_id` 置空与删目录在同一个写事务里完成，
/// 中途失败不会留下「指向已删目录」的笔记。
fn delete_folder_with_connection(connection: &Connection, id: i64) -> Result<bool, String> {
    let transaction = Transaction::new_unchecked(connection, TransactionBehavior::Immediate)
        .map_err(|error| format!("开始删除目录事务失败: {error}"))?;
    let exists: Option<i64> = transaction
        .query_row(
            "SELECT id FROM note_folders WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| format!("读取待删除目录失败: {error}"))?;
    if exists.is_none() {
        return Ok(false);
    }
    transaction
        .execute(
            "UPDATE notes SET folder_id = NULL, updated_at = ?2 WHERE folder_id = ?1",
            params![id, now_millis()],
        )
        .map_err(|error| format!("解除笔记目录归属失败: {error}"))?;
    transaction
        .execute("DELETE FROM note_folders WHERE id = ?1", params![id])
        .map_err(|error| format!("删除目录失败: {error}"))?;
    transaction
        .commit()
        .map_err(|error| format!("提交删除目录事务失败: {error}"))?;
    Ok(true)
}

/// 按传入顺序重排目录；未出现在列表里的目录保持原有相对顺序。
fn reorder_folders_with_connection(connection: &Connection, ids: &[i64]) -> Result<(), String> {
    let transaction = Transaction::new_unchecked(connection, TransactionBehavior::Immediate)
        .map_err(|error| format!("开始排序目录事务失败: {error}"))?;
    let now = now_millis();
    for (index, id) in ids.iter().enumerate() {
        transaction
            .execute(
                "UPDATE note_folders SET sort_order = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, index as i64, now],
            )
            .map_err(|error| format!("更新目录排序失败: {error}"))?;
    }
    transaction
        .commit()
        .map_err(|error| format!("提交排序目录事务失败: {error}"))
}

// ---------------------------------------------------------------------------
// 笔记
// ---------------------------------------------------------------------------

/// 列表列：摘要优先取正文，正文为空时退回转录；`has_*` 用 SQL 判定，避免把全文拉过来。
const LIST_ITEM_COLUMNS: &str = "n.id, n.folder_id, n.title, n.kind, \
     substr(CASE WHEN n.content <> '' THEN n.content ELSE COALESCE(n.transcript, '') END, 1, 140), \
     n.duration_sec, \
     (CASE WHEN n.transcript IS NOT NULL AND trim(n.transcript) <> '' THEN 1 ELSE 0 END), \
     (CASE WHEN n.summary IS NOT NULL AND trim(n.summary) <> '' THEN 1 ELSE 0 END), \
     n.audio_file, n.created_at, n.updated_at";

const DETAIL_COLUMNS: &str = "n.id, n.folder_id, n.title, n.content, n.transcript, n.summary, \
     n.summary_model, n.summary_generated_at, n.summary_source_hash, n.kind, n.duration_sec, \
     n.history_id, n.audio_file, n.created_at, n.updated_at";

fn list_item_from_row(row: &Row<'_>) -> rusqlite::Result<NoteListItem> {
    Ok(NoteListItem {
        id: row.get(0)?,
        folder_id: row.get(1)?,
        title: row.get(2)?,
        kind: row.get(3)?,
        excerpt: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
        duration_sec: row.get(5)?,
        has_transcript: row.get::<_, i64>(6)? != 0,
        has_summary: row.get::<_, i64>(7)? != 0,
        has_audio: row.get::<_, Option<String>>(8)?.is_some(),
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
    })
}

fn detail_from_row(row: &Row<'_>) -> rusqlite::Result<NoteDetail> {
    Ok(NoteDetail {
        id: row.get(0)?,
        folder_id: row.get(1)?,
        title: row.get(2)?,
        content: row.get(3)?,
        transcript: row.get(4)?,
        summary: row.get(5)?,
        summary_model: row.get(6)?,
        summary_generated_at: row.get(7)?,
        summary_source_hash: row.get(8)?,
        kind: row.get(9)?,
        duration_sec: row.get(10)?,
        history_id: row.get(11)?,
        audio_file: row.get(12)?,
        created_at: row.get(13)?,
        updated_at: row.get(14)?,
    })
}

/// `LIKE` 转义：查询词里的 `%`、`_`、`\` 要当普通字符处理，否则用户搜「100%」会匹配全部。
fn escape_like(query: &str) -> String {
    let mut escaped = String::with_capacity(query.len() + 8);
    for character in query.chars() {
        match character {
            '\\' | '%' | '_' => {
                escaped.push('\\');
                escaped.push(character);
            }
            other => escaped.push(other),
        }
    }
    escaped
}

/// 目录/搜索过滤条件。占位符顺序与 `args` 一致，计数与列表查询共用同一份。
fn build_note_filter(query: &NoteQuery) -> (String, Vec<Value>) {
    let mut filter = String::new();
    let mut args: Vec<Value> = Vec::new();
    if query.unfiled {
        filter.push_str(" AND n.folder_id IS NULL");
    } else if let Some(folder_id) = query.folder_id {
        filter.push_str(" AND n.folder_id = ?");
        args.push(Value::Integer(folder_id));
    }
    let term = escape_like(query.query.trim());
    if !term.is_empty() {
        filter.push_str(
            " AND (n.title LIKE ? ESCAPE '\\' OR n.content LIKE ? ESCAPE '\\' \
               OR COALESCE(n.transcript, '') LIKE ? ESCAPE '\\')",
        );
        let pattern = Value::Text(format!("%{term}%"));
        args.extend([pattern.clone(), pattern.clone(), pattern]);
    }
    (filter, args)
}

fn list_notes_with_connection(
    connection: &Connection,
    query: &NoteQuery,
) -> Result<NotePage, String> {
    let (filter, args) = build_note_filter(query);
    let count_sql = format!("SELECT COUNT(*) FROM notes n WHERE 1 = 1{filter}");
    let total: i64 = connection
        .query_row(&count_sql, params_from_iter(args.clone()), |row| row.get(0))
        .map_err(|error| format!("统计笔记失败: {error}"))?;

    let limit = query.limit.clamp(1, MAX_PAGE_SIZE);
    let list_sql = format!(
        "SELECT {LIST_ITEM_COLUMNS} FROM notes n WHERE 1 = 1{filter} \
         ORDER BY n.updated_at DESC, n.id DESC LIMIT ? OFFSET ?"
    );
    let mut list_args = args;
    list_args.push(Value::Integer(i64::from(limit)));
    list_args.push(Value::Integer(i64::from(query.offset)));
    let mut statement = connection
        .prepare(&list_sql)
        .map_err(|error| format!("准备查询笔记失败: {error}"))?;
    let rows = statement
        .query_map(params_from_iter(list_args), list_item_from_row)
        .map_err(|error| format!("查询笔记失败: {error}"))?;
    let items = rows
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| format!("解析笔记失败: {error}"))?;
    let total = u64::try_from(total).unwrap_or(0);
    Ok(NotePage {
        has_more: u64::from(query.offset) + (items.len() as u64) < total,
        items,
        total,
    })
}

fn normalize_kind(kind: &str) -> String {
    if NOTE_KINDS.contains(&kind) {
        kind.to_string()
    } else {
        "personal".to_string()
    }
}

fn folder_exists(connection: &Connection, id: i64) -> Result<bool, String> {
    let found: Option<i64> = connection
        .query_row(
            "SELECT id FROM note_folders WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| format!("检查目录是否存在失败: {error}"))?;
    Ok(found.is_some())
}

fn get_note_with_connection(
    connection: &Connection,
    id: i64,
) -> Result<Option<NoteDetail>, String> {
    let sql = format!("SELECT {DETAIL_COLUMNS} FROM notes n WHERE n.id = ?1");
    connection
        .query_row(&sql, params![id], detail_from_row)
        .optional()
        .map_err(|error| format!("读取笔记失败: {error}"))
}

fn insert_note_with_connection(
    connection: &Connection,
    folder_id: Option<i64>,
    title: &str,
    kind: &str,
) -> Result<NoteDetail, String> {
    if let Some(id) = folder_id {
        if !folder_exists(connection, id)? {
            return Err("目录不存在".into());
        }
    }
    let now = now_millis();
    connection
        .execute(
            "INSERT INTO notes (folder_id, title, content, kind, created_at, updated_at) \
             VALUES (?1, ?2, '', ?3, ?4, ?4)",
            params![folder_id, title.trim(), normalize_kind(kind), now],
        )
        .map_err(|error| format!("创建笔记失败: {error}"))?;
    get_note_with_connection(connection, connection.last_insert_rowid())?
        .ok_or_else(|| "笔记创建后读取失败".into())
}

fn update_note_with_connection(
    connection: &Connection,
    id: i64,
    patch: &NotePatch,
) -> Result<Option<NoteDetail>, String> {
    if patch.title.is_none() && patch.content.is_none() {
        return get_note_with_connection(connection, id);
    }
    let mut assignments: Vec<&str> = Vec::new();
    let mut values: Vec<Value> = Vec::new();
    if let Some(title) = patch.title.as_ref() {
        assignments.push("title = ?");
        values.push(Value::Text(title.trim().to_string()));
    }
    if let Some(content) = patch.content.as_ref() {
        assignments.push("content = ?");
        values.push(Value::Text(content.clone()));
    }
    assignments.push("updated_at = ?");
    values.push(Value::Integer(now_millis()));
    values.push(Value::Integer(id));
    let sql = format!("UPDATE notes SET {} WHERE id = ?", assignments.join(", "));
    let changed = connection
        .execute(&sql, params_from_iter(values))
        .map_err(|error| format!("保存笔记失败: {error}"))?;
    if changed == 0 {
        return Ok(None);
    }
    get_note_with_connection(connection, id)
}

fn move_note_with_connection(
    connection: &Connection,
    id: i64,
    folder_id: Option<i64>,
) -> Result<Option<NoteDetail>, String> {
    if let Some(target) = folder_id {
        if !folder_exists(connection, target)? {
            return Err("目录不存在".into());
        }
    }
    let changed = connection
        .execute(
            "UPDATE notes SET folder_id = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, folder_id, now_millis()],
        )
        .map_err(|error| format!("移动笔记失败: {error}"))?;
    if changed == 0 {
        return Ok(None);
    }
    get_note_with_connection(connection, id)
}

/// 删笔记，返回被删笔记引用的音频文件名（外层 `None` = 笔记不存在）。
fn delete_note_with_connection(
    connection: &Connection,
    id: i64,
) -> Result<Option<Option<String>>, String> {
    let audio_file: Option<String> = connection
        .query_row(
            "SELECT audio_file FROM notes WHERE id = ?1",
            params![id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .map_err(|error| format!("读取待删除笔记失败: {error}"))?
        .flatten();
    let changed = connection
        .execute("DELETE FROM notes WHERE id = ?1", params![id])
        .map_err(|error| format!("删除笔记失败: {error}"))?;
    if changed == 0 {
        return Ok(None);
    }
    Ok(Some(audio_file))
}

fn set_transcript_with_connection(
    connection: &Connection,
    id: i64,
    transcript: &str,
    duration_sec: Option<f64>,
    history_id: Option<i64>,
) -> Result<Option<NoteDetail>, String> {
    let changed = connection
        .execute(
            "UPDATE notes SET transcript = ?2, \
                duration_sec = COALESCE(?3, duration_sec), \
                history_id = COALESCE(?4, history_id), \
                updated_at = ?5 \
             WHERE id = ?1",
            params![id, transcript, duration_sec, history_id, now_millis()],
        )
        .map_err(|error| format!("保存笔记转录失败: {error}"))?;
    if changed == 0 {
        return Ok(None);
    }
    get_note_with_connection(connection, id)
}

fn find_note_by_history_with_connection(
    connection: &Connection,
    history_id: i64,
) -> Result<Option<NoteDetail>, String> {
    let sql = format!(
        "SELECT {DETAIL_COLUMNS} FROM notes n WHERE n.history_id = ?1 ORDER BY n.id LIMIT 1"
    );
    connection
        .query_row(&sql, params![history_id], detail_from_row)
        .optional()
        .map_err(|error| format!("读取笔记失败: {error}"))
}

fn set_audio_with_connection(
    connection: &Connection,
    id: i64,
    audio_file: Option<&str>,
    duration_sec: Option<f64>,
) -> Result<Option<NoteDetail>, String> {
    let changed = connection
        .execute(
            "UPDATE notes SET audio_file = ?2, \
                duration_sec = COALESCE(?3, duration_sec), updated_at = ?4 \
             WHERE id = ?1",
            params![id, audio_file, duration_sec, now_millis()],
        )
        .map_err(|error| format!("保存笔记录音失败: {error}"))?;
    if changed == 0 {
        return Ok(None);
    }
    get_note_with_connection(connection, id)
}

// ---------------------------------------------------------------------------
// 对外接口
// ---------------------------------------------------------------------------

pub async fn list_folders() -> Result<Vec<NoteFolder>, String> {
    initialize().await?;
    tokio::task::spawn_blocking(|| {
        let connection = open_connection()?;
        list_folders_with_connection(&connection)
    })
    .await
    .map_err(|error| format!("查询笔记目录任务失败: {error}"))?
}

pub async fn create_folder(name: &str) -> Result<NoteFolder, String> {
    initialize().await?;
    let name = name.to_string();
    tokio::task::spawn_blocking(move || {
        let connection = open_connection()?;
        insert_folder_with_connection(&connection, &name)
    })
    .await
    .map_err(|error| format!("创建笔记目录任务失败: {error}"))?
}

pub async fn rename_folder(id: i64, name: &str) -> Result<NoteFolder, String> {
    initialize().await?;
    let name = name.to_string();
    tokio::task::spawn_blocking(move || {
        let connection = open_connection()?;
        rename_folder_with_connection(&connection, id, &name)
    })
    .await
    .map_err(|error| format!("重命名笔记目录任务失败: {error}"))?
}

pub async fn delete_folder(id: i64) -> Result<bool, String> {
    initialize().await?;
    tokio::task::spawn_blocking(move || {
        let connection = open_connection()?;
        delete_folder_with_connection(&connection, id)
    })
    .await
    .map_err(|error| format!("删除笔记目录任务失败: {error}"))?
}

pub async fn reorder_folders(ids: Vec<i64>) -> Result<Vec<NoteFolder>, String> {
    initialize().await?;
    tokio::task::spawn_blocking(move || {
        let connection = open_connection()?;
        reorder_folders_with_connection(&connection, &ids)?;
        list_folders_with_connection(&connection)
    })
    .await
    .map_err(|error| format!("排序笔记目录任务失败: {error}"))?
}

pub async fn list_notes(query: NoteQuery) -> Result<NotePage, String> {
    initialize().await?;
    tokio::task::spawn_blocking(move || {
        let connection = open_connection()?;
        list_notes_with_connection(&connection, &query)
    })
    .await
    .map_err(|error| format!("查询笔记任务失败: {error}"))?
}

pub async fn get_note(id: i64) -> Result<Option<NoteDetail>, String> {
    initialize().await?;
    tokio::task::spawn_blocking(move || {
        let connection = open_connection()?;
        get_note_with_connection(&connection, id)
    })
    .await
    .map_err(|error| format!("读取笔记任务失败: {error}"))?
}

/// 所有被笔记引用的音频文件名。
///
/// 只给启动清理用：录音目录里不在这份集合中的文件就是没人认领的残留。
pub async fn audio_file_names() -> Result<std::collections::HashSet<String>, String> {
    initialize().await?;
    tokio::task::spawn_blocking(move || {
        let connection = open_connection()?;
        audio_file_names_with_connection(&connection)
    })
    .await
    .map_err(|error| format!("查询笔记录音引用任务失败: {error}"))?
}

fn audio_file_names_with_connection(
    connection: &Connection,
) -> Result<std::collections::HashSet<String>, String> {
    let mut statement = connection
        .prepare("SELECT audio_file FROM notes WHERE audio_file IS NOT NULL")
        .map_err(|error| format!("准备查询笔记录音引用失败: {error}"))?;
    let rows = statement
        .query_map([], |row| row.get::<_, Option<String>>(0))
        .map_err(|error| format!("查询笔记录音引用失败: {error}"))?;
    let mut names = std::collections::HashSet::new();
    for row in rows {
        if let Some(name) = row.map_err(|error| format!("解析笔记录音引用失败: {error}"))? {
            names.insert(name);
        }
    }
    Ok(names)
}

pub async fn create_note(
    folder_id: Option<i64>,
    title: &str,
    kind: &str,
) -> Result<NoteDetail, String> {
    initialize().await?;
    let title = title.to_string();
    let kind = kind.to_string();
    tokio::task::spawn_blocking(move || {
        let connection = open_connection()?;
        insert_note_with_connection(&connection, folder_id, &title, &kind)
    })
    .await
    .map_err(|error| format!("创建笔记任务失败: {error}"))?
}

pub async fn update_note(id: i64, patch: NotePatch) -> Result<Option<NoteDetail>, String> {
    initialize().await?;
    tokio::task::spawn_blocking(move || {
        let connection = open_connection()?;
        update_note_with_connection(&connection, id, &patch)
    })
    .await
    .map_err(|error| format!("保存笔记任务失败: {error}"))?
}

pub async fn move_note(id: i64, folder_id: Option<i64>) -> Result<Option<NoteDetail>, String> {
    initialize().await?;
    tokio::task::spawn_blocking(move || {
        let connection = open_connection()?;
        move_note_with_connection(&connection, id, folder_id)
    })
    .await
    .map_err(|error| format!("移动笔记任务失败: {error}"))?
}

/// 删除笔记，返回被删除笔记引用的音频文件名（没有则 `None`）。
///
/// 音频文件的实际清理由录音模块负责，这里只把引用交回去。
pub async fn delete_note(id: i64) -> Result<Option<String>, String> {
    initialize().await?;
    tokio::task::spawn_blocking(move || {
        let connection = open_connection()?;
        Ok(delete_note_with_connection(&connection, id)?.flatten())
    })
    .await
    .map_err(|error| format!("删除笔记任务失败: {error}"))?
}

/// 用转录覆盖笔记的转录与时长，并挂上历史记录 ID。
///
/// 只由转写流程（`note_transcription`）调用；`history_id` 传 `None` 表示保持原值不变。
pub async fn set_transcript(
    id: i64,
    transcript: &str,
    duration_sec: Option<f64>,
    history_id: Option<i64>,
) -> Result<Option<NoteDetail>, String> {
    initialize().await?;
    let transcript = transcript.to_string();
    tokio::task::spawn_blocking(move || {
        let connection = open_connection()?;
        set_transcript_with_connection(&connection, id, &transcript, duration_sec, history_id)
    })
    .await
    .map_err(|error| format!("保存笔记转录任务失败: {error}"))?
}

/// 记录录音结果（录音模块使用）：只写文件名与时长，不动转录。
pub async fn set_audio(
    id: i64,
    audio_file: Option<&str>,
    duration_sec: Option<f64>,
) -> Result<Option<NoteDetail>, String> {
    initialize().await?;
    let audio_file = audio_file.map(str::to_string);
    tokio::task::spawn_blocking(move || {
        let connection = open_connection()?;
        set_audio_with_connection(&connection, id, audio_file.as_deref(), duration_sec)
    })
    .await
    .map_err(|error| format!("保存笔记录音任务失败: {error}"))?
}

fn create_note_from_transcript_with_connection(
    connection: &Connection,
    folder_id: Option<i64>,
    title: &str,
    kind: &str,
    transcript: &str,
    duration_sec: Option<f64>,
    history_id: i64,
) -> Result<(NoteDetail, bool), String> {
    if let Some(existing) = find_note_by_history_with_connection(connection, history_id)? {
        return Ok((existing, false));
    }
    let created = insert_note_with_connection(connection, folder_id, title, kind)?;
    let detail = set_transcript_with_connection(
        connection,
        created.id,
        transcript,
        duration_sec,
        Some(history_id),
    )?
    .ok_or_else(|| "写入笔记转录失败：笔记不存在".to_string())?;
    Ok((detail, true))
}

/// 从历史记录生成笔记：建笔记 + 写转录在同一次连接内完成，避免半成品。
///
/// 同一条历史记录重复调用返回已有笔记（第二个返回值为 `false`），连点不会
/// 产生多条重复笔记。音频不复制，由 `history_id` 反向引用历史记录。
pub async fn create_note_from_transcript(
    folder_id: Option<i64>,
    title: &str,
    kind: &str,
    transcript: &str,
    duration_sec: Option<f64>,
    history_id: i64,
) -> Result<(NoteDetail, bool), String> {
    initialize().await?;
    let title = title.to_string();
    let kind = kind.to_string();
    let transcript = transcript.to_string();
    tokio::task::spawn_blocking(move || {
        let connection = open_connection()?;
        create_note_from_transcript_with_connection(
            &connection,
            folder_id,
            &title,
            &kind,
            &transcript,
            duration_sec,
            history_id,
        )
    })
    .await
    .map_err(|error| format!("创建笔记任务失败: {error}"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory_connection() -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        migrate_schema(&connection).unwrap();
        connection
    }

    fn query(folder_id: Option<i64>, unfiled: bool, term: &str) -> NoteQuery {
        NoteQuery {
            folder_id,
            unfiled,
            query: term.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn folders_reject_blank_duplicate_and_overlong_names() {
        let connection = memory_connection();
        assert!(insert_folder_with_connection(&connection, "   ").is_err());
        assert!(insert_folder_with_connection(&connection, &"长".repeat(61)).is_err());
        let folder = insert_folder_with_connection(&connection, " 会议 ").unwrap();
        assert_eq!(folder.name, "会议");
        assert!(insert_folder_with_connection(&connection, "会议").is_err());
    }

    #[test]
    fn deleting_a_folder_unfiles_its_notes_without_touching_others() {
        let connection = memory_connection();
        let meeting = insert_folder_with_connection(&connection, "会议").unwrap();
        let personal = insert_folder_with_connection(&connection, "个人").unwrap();
        let inside =
            insert_note_with_connection(&connection, Some(meeting.id), "周会", "meeting").unwrap();
        let outside =
            insert_note_with_connection(&connection, Some(personal.id), "灵感", "personal")
                .unwrap();

        assert!(delete_folder_with_connection(&connection, meeting.id).unwrap());
        assert!(get_note_with_connection(&connection, inside.id)
            .unwrap()
            .unwrap()
            .folder_id
            .is_none());
        assert_eq!(
            get_note_with_connection(&connection, outside.id)
                .unwrap()
                .unwrap()
                .folder_id,
            Some(personal.id)
        );
        assert!(!delete_folder_with_connection(&connection, meeting.id).unwrap());
    }

    #[test]
    fn reorder_folders_applies_the_given_order() {
        let connection = memory_connection();
        let first = insert_folder_with_connection(&connection, "A").unwrap();
        let second = insert_folder_with_connection(&connection, "B").unwrap();
        let third = insert_folder_with_connection(&connection, "C").unwrap();

        reorder_folders_with_connection(&connection, &[third.id, first.id, second.id]).unwrap();
        let names: Vec<String> = list_folders_with_connection(&connection)
            .unwrap()
            .into_iter()
            .map(|folder| folder.name)
            .collect();
        assert_eq!(names, vec!["C".to_string(), "A".into(), "B".into()]);
    }

    #[test]
    fn folder_note_count_tracks_moves_and_deletions() {
        let connection = memory_connection();
        let folder = insert_folder_with_connection(&connection, "会议").unwrap();
        let note =
            insert_note_with_connection(&connection, Some(folder.id), "周会", "meeting").unwrap();
        assert_eq!(
            list_folders_with_connection(&connection).unwrap()[0].note_count,
            1
        );

        move_note_with_connection(&connection, note.id, None).unwrap();
        assert_eq!(
            list_folders_with_connection(&connection).unwrap()[0].note_count,
            0
        );
    }

    #[test]
    fn list_notes_filters_by_folder_unfiled_and_search_term() {
        let connection = memory_connection();
        let folder = insert_folder_with_connection(&connection, "会议").unwrap();
        let filed =
            insert_note_with_connection(&connection, Some(folder.id), "产品评审", "meeting")
                .unwrap();
        let unfiled = insert_note_with_connection(&connection, None, "随手记", "personal").unwrap();
        update_note_with_connection(
            &connection,
            filed.id,
            &NotePatch {
                title: Some("产品评审".into()),
                content: Some("讨论了 100% 的覆盖率目标".into()),
            },
        )
        .unwrap();
        set_transcript_with_connection(
            &connection,
            unfiled.id,
            "录音转写内容",
            Some(12.5),
            Some(7),
        )
        .unwrap();

        assert_eq!(
            list_notes_with_connection(&connection, &query(None, false, ""))
                .unwrap()
                .total,
            2
        );

        let in_folder =
            list_notes_with_connection(&connection, &query(Some(folder.id), false, "")).unwrap();
        assert_eq!(in_folder.items.len(), 1);
        assert_eq!(in_folder.items[0].id, filed.id);

        // folder 与 unfiled 同时给出时以 unfiled 为准，避免必然为空的交集。
        let only_unfiled =
            list_notes_with_connection(&connection, &query(Some(folder.id), true, "")).unwrap();
        assert_eq!(only_unfiled.items.len(), 1);
        assert_eq!(only_unfiled.items[0].id, unfiled.id);
        assert!(only_unfiled.items[0].has_transcript);
        assert_eq!(only_unfiled.items[0].duration_sec, Some(12.5));

        let percent = list_notes_with_connection(&connection, &query(None, false, "100%")).unwrap();
        assert_eq!(percent.items.len(), 1);
        assert_eq!(percent.items[0].id, filed.id);

        let by_transcript =
            list_notes_with_connection(&connection, &query(None, false, "录音转写")).unwrap();
        assert_eq!(by_transcript.items.len(), 1);
        assert_eq!(by_transcript.items[0].id, unfiled.id);
    }

    #[test]
    fn underscore_in_query_is_a_literal_not_a_wildcard() {
        let connection = memory_connection();
        let matching = insert_note_with_connection(&connection, None, "a_b", "personal").unwrap();
        insert_note_with_connection(&connection, None, "axb", "personal").unwrap();

        let page = list_notes_with_connection(&connection, &query(None, false, "a_b")).unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].id, matching.id);
    }

    #[test]
    fn pagination_reports_total_and_has_more() {
        let connection = memory_connection();
        for index in 0..5 {
            insert_note_with_connection(&connection, None, &format!("笔记 {index}"), "personal")
                .unwrap();
        }
        let first = list_notes_with_connection(
            &connection,
            &NoteQuery {
                limit: 2,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(first.total, 5);
        assert_eq!(first.items.len(), 2);
        assert!(first.has_more);

        let last = list_notes_with_connection(
            &connection,
            &NoteQuery {
                limit: 2,
                offset: 4,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(last.items.len(), 1);
        assert!(!last.has_more);
    }

    #[test]
    fn update_note_only_touches_supplied_fields() {
        let connection = memory_connection();
        let note = insert_note_with_connection(&connection, None, "原标题", "personal").unwrap();
        update_note_with_connection(
            &connection,
            note.id,
            &NotePatch {
                title: None,
                content: Some("正文".into()),
            },
        )
        .unwrap();
        let after_content = get_note_with_connection(&connection, note.id)
            .unwrap()
            .unwrap();
        assert_eq!(after_content.title, "原标题");
        assert_eq!(after_content.content, "正文");

        update_note_with_connection(
            &connection,
            note.id,
            &NotePatch {
                title: Some("  新标题  ".into()),
                content: None,
            },
        )
        .unwrap();
        let after_title = get_note_with_connection(&connection, note.id)
            .unwrap()
            .unwrap();
        assert_eq!(after_title.title, "新标题");
        assert_eq!(after_title.content, "正文");
    }

    #[test]
    fn create_note_rejects_unknown_folder_and_normalizes_kind() {
        let connection = memory_connection();
        assert!(insert_note_with_connection(&connection, Some(999), "x", "personal").is_err());
        let note = insert_note_with_connection(&connection, None, "x", "unknown-kind").unwrap();
        assert_eq!(note.kind, "personal");
        assert_eq!(note.content, "");
    }

    #[test]
    fn delete_note_returns_audio_reference_for_the_caller_to_clean() {
        let connection = memory_connection();
        let note = insert_note_with_connection(&connection, None, "会议", "meeting").unwrap();
        set_audio_with_connection(&connection, note.id, Some("1-123.wav"), Some(30.0)).unwrap();
        assert_eq!(
            delete_note_with_connection(&connection, note.id).unwrap(),
            Some(Some("1-123.wav".to_string()))
        );
        assert!(delete_note_with_connection(&connection, note.id)
            .unwrap()
            .is_none());
    }

    #[test]
    fn excerpt_prefers_content_then_transcript() {
        let connection = memory_connection();
        let note = insert_note_with_connection(&connection, None, "标题", "meeting").unwrap();
        set_transcript_with_connection(&connection, note.id, "转录正文", None, None).unwrap();
        let page = list_notes_with_connection(&connection, &query(None, false, "")).unwrap();
        assert_eq!(page.items[0].excerpt, "转录正文");
        assert!(page.items[0].has_transcript);
        assert!(!page.items[0].has_summary);
        assert!(!page.items[0].has_audio);
    }

    #[test]
    fn audio_file_names_lists_only_referenced_recordings() {
        let connection = memory_connection();
        let with_audio = insert_note_with_connection(&connection, None, "有录音", "meeting").unwrap();
        let without_audio = insert_note_with_connection(&connection, None, "无录音", "personal").unwrap();
        set_audio_with_connection(&connection, with_audio.id, Some("note-1-2.wav"), Some(12.5))
            .unwrap();

        let names = audio_file_names_with_connection(&connection).unwrap();
        assert_eq!(names.len(), 1);
        assert!(names.contains("note-1-2.wav"));
        assert!(!names.iter().any(|name| name.is_empty()));

        // 删掉唯一的引用后集合必须为空，否则启动清理不敢删任何文件。
        delete_note_with_connection(&connection, with_audio.id).unwrap();
        assert!(audio_file_names_with_connection(&connection).unwrap().is_empty());
        assert!(get_note_with_connection(&connection, without_audio.id)
            .unwrap()
            .is_some());
    }

    #[test]
    fn set_audio_records_duration_without_touching_transcript() {
        let connection = memory_connection();
        let note = insert_note_with_connection(&connection, None, "会议", "meeting").unwrap();
        set_transcript_with_connection(&connection, note.id, "已有转录", None, None).unwrap();
        let updated = set_audio_with_connection(&connection, note.id, Some("a.wav"), Some(61.5))
            .unwrap()
            .unwrap();
        assert_eq!(updated.audio_file.as_deref(), Some("a.wav"));
        assert_eq!(updated.duration_sec, Some(61.5));
        assert_eq!(updated.transcript.as_deref(), Some("已有转录"));
    }

    #[test]
    fn rename_folder_rejects_duplicates_but_allows_itself() {
        let connection = memory_connection();
        let folder = insert_folder_with_connection(&connection, "会议").unwrap();
        let other = insert_folder_with_connection(&connection, "项目").unwrap();
        assert!(rename_folder_with_connection(&connection, other.id, "会议").is_err());
        let renamed = rename_folder_with_connection(&connection, folder.id, "会议纪要").unwrap();
        assert_eq!(renamed.name, "会议纪要");
        assert!(rename_folder_with_connection(&connection, 999, "任意").is_err());
    }

    #[test]
    fn note_from_history_carries_transcript_and_duration() {
        let connection = memory_connection();
        let folder = insert_folder_with_connection(&connection, "会议").unwrap();
        let (note, created) = create_note_from_transcript_with_connection(
            &connection,
            Some(folder.id),
            "周会",
            "import",
            "第一段\n\n第二段",
            Some(1830.5),
            42,
        )
        .unwrap();

        assert!(created);
        assert_eq!(note.title, "周会");
        assert_eq!(note.kind, "import");
        assert_eq!(note.folder_id, Some(folder.id));
        assert_eq!(note.transcript.as_deref(), Some("第一段\n\n第二段"));
        assert_eq!(note.duration_sec, Some(1830.5));
        assert_eq!(note.history_id, Some(42));
        assert!(note.content.is_empty());
    }

    /// 连点两次「存为笔记」不能产生两条笔记，第二次返回已有那条。
    #[test]
    fn note_from_history_is_idempotent_per_history_record() {
        let connection = memory_connection();
        let (first, created) = create_note_from_transcript_with_connection(
            &connection, None, "周会", "import", "转录", None, 7,
        )
        .unwrap();
        let (second, created_again) = create_note_from_transcript_with_connection(
            &connection, None, "周会", "import", "转录", None, 7,
        )
        .unwrap();

        assert!(created);
        assert!(!created_again);
        assert_eq!(first.id, second.id);
        let page = list_notes_with_connection(&connection, &query(None, false, "")).unwrap();
        assert_eq!(page.total, 1);
    }
}
