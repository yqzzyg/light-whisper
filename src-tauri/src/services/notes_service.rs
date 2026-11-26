//! 笔记与笔记目录的持久化。
//!
//! 与历史模块同在 `db` 的数据库文件里，表分属各自模块。同库是为了拿到跨模块的事务
//! 与外键：一次笔记转写要同时写历史行和笔记 transcript，「历史 → 存为笔记」要读
//! 一侧写另一侧，这两处必须能一起回滚、能约束引用。
//!
//! `notes.history_id` 与 `note_recordings.history_id` 都指向 `transcription_history(id)`，
//! `ON DELETE SET NULL`：历史被保留期淘汰时只断开引用，笔记本身不受影响。
//!
//! 约定：
//!
//! - 目录只有一层，`notes.folder_id` 为 `NULL` 表示「未分类」。
//! - 删除目录不删笔记，只把目录内笔记的 `folder_id` 置空，且与删目录在同一个写事务里完成。
//! - `title` 允许为空，展示层兜底成「未命名笔记」，不在库里写死语言相关文案。
//! - 文本内容只在这里落库；音频文件不归本模块管（见 `note_recording_service`），本模块只存文件名。

use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::types::Value;
use rusqlite::{
    params, params_from_iter, Connection, OptionalExtension, Row, Transaction, TransactionBehavior,
};
use serde::{Deserialize, Serialize};
use tokio::sync::OnceCell;

use crate::db;
use crate::services::history_service;

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

fn ensure_schema(connection: &Connection) -> Result<(), String> {
    // `notes` 与 `note_recordings` 的 `history_id` 是指向 `transcription_history` 的外键，
    // SQLite 建表时会校验父表存在，所以历史模块必须先建。两步各自开事务，不能嵌套。
    history_service::ensure_schema(connection)?;
    db::ensure_schema(connection, |transaction| {
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
                    history_id           INTEGER REFERENCES transcription_history(id) ON DELETE SET NULL,
                    created_at           INTEGER NOT NULL,
                    updated_at           INTEGER NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_notes_folder  ON notes(folder_id, updated_at DESC);
                CREATE INDEX IF NOT EXISTS idx_notes_updated ON notes(updated_at DESC);
                -- 一条历史只能转出一篇笔记。应用层已经查重，这里让引擎也守住，
                -- 避免并发连点漏出两条。
                CREATE UNIQUE INDEX IF NOT EXISTS idx_notes_history
                    ON notes(history_id) WHERE history_id IS NOT NULL;
                CREATE TABLE IF NOT EXISTS note_recordings (
                    id           INTEGER PRIMARY KEY AUTOINCREMENT,
                    note_id      INTEGER NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
                    audio_file   TEXT    NOT NULL,
                    duration_sec REAL,
                    transcript   TEXT,
                    history_id   INTEGER REFERENCES transcription_history(id) ON DELETE SET NULL,
                    sort_order   INTEGER NOT NULL DEFAULT 0,
                    created_at   INTEGER NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_note_recordings_note
                    ON note_recordings(note_id, sort_order, id);
                -- 说话人分离的分段结果。一行录音对应多行分段，录音删了级联删。
                -- `speaker_id` 是「本次录音内」的聚类编号，跨录音没有可比性。
                CREATE TABLE IF NOT EXISTS note_recording_segments (
                    id           INTEGER PRIMARY KEY AUTOINCREMENT,
                    recording_id INTEGER NOT NULL REFERENCES note_recordings(id) ON DELETE CASCADE,
                    speaker_id   INTEGER NOT NULL DEFAULT 0,
                    start_ms     INTEGER NOT NULL,
                    end_ms       INTEGER NOT NULL,
                    text         TEXT    NOT NULL DEFAULT '',
                    overlap      INTEGER NOT NULL DEFAULT 0,
                    sort_order   INTEGER NOT NULL DEFAULT 0,
                    created_at   INTEGER NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_note_recording_segments
                    ON note_recording_segments(recording_id, sort_order);
                -- 说话人显示名。P2 才用；P0/P1 由前端按 speaker_id 生成「说话人 N」。
                CREATE TABLE IF NOT EXISTS note_recording_speakers (
                    id           INTEGER PRIMARY KEY AUTOINCREMENT,
                    recording_id INTEGER NOT NULL REFERENCES note_recordings(id) ON DELETE CASCADE,
                    speaker_id   INTEGER NOT NULL,
                    label        TEXT    NOT NULL DEFAULT '',
                    created_at   INTEGER NOT NULL
                );
                CREATE UNIQUE INDEX IF NOT EXISTS idx_note_recording_speakers
                    ON note_recording_speakers(recording_id, speaker_id);
                "#,
            )
            .map_err(|error| format!("初始化笔记数据库失败: {error}"))
    })
}

/// 幂等初始化。所有对外函数都先调它，与历史模块保持同一范式。
///
/// 只跑 `ensure_schema`（含历史侧的建表），**不**跑 `history_service::initialize`：
/// 后者还会清租约与孤儿历史音频，那是历史模块自己的启动清理，由 `lib.rs` 单独触发。
pub async fn initialize() -> Result<(), String> {
    NOTES_INITIALIZED
        .get_or_try_init(|| async {
            tokio::task::spawn_blocking(|| {
                let connection = db::open()?;
                ensure_schema(&connection)
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
    /// 转录或正文在摘要生成后变过。比对放在后端，指纹算法只在一处实现。
    pub summary_stale: bool,
    pub kind: String,
    pub duration_sec: Option<f64>,
    pub history_id: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 笔记里的一段录音。一段 = 一次 start/stop，独立文件、独立转录与历史引用。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NoteRecording {
    pub id: i64,
    pub note_id: i64,
    pub audio_file: String,
    pub duration_sec: Option<f64>,
    pub transcript: Option<String>,
    pub history_id: Option<i64>,
    pub sort_order: i64,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotePage {
    pub items: Vec<NoteListItem>,
    pub total: u64,
    pub has_more: bool,
}

/// 说话人分段沿用历史模块的定义。
///
/// 分段本来就不是笔记专有的：导入音频的历史同样会有。字段保持 snake_case
/// （与 Python 侧协议一致），这里只做重导出，让既有调用点不必改路径。
pub use crate::services::history_service::SpeakerSegment;

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
    (CASE WHEN EXISTS (SELECT 1 FROM note_recordings r WHERE r.note_id = n.id) THEN 1 ELSE 0 END), \
    n.created_at, n.updated_at";

const DETAIL_COLUMNS: &str = "n.id, n.folder_id, n.title, n.content, n.transcript, n.summary, \
     n.summary_model, n.summary_generated_at, n.summary_source_hash, n.kind, n.duration_sec, \
     n.history_id, n.created_at, n.updated_at";

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
        has_audio: row.get::<_, i64>(8)? != 0,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
    })
}

/// 摘要指纹：转录或手写正文任一变化，旧摘要就该被标成「过期」。
///
/// 放在这里而不是摘要服务里：比对发生在读笔记时（`detail_from_row`），写库时也
/// 由本模块落列；`note_summary_service` 反过来调用本函数，依赖保持单向
/// （业务层 → 存储层），不会两头互指。
pub fn source_hash(transcript: &str, content: &str) -> String {
    // FNV-1a：短、稳定、不引依赖。分隔符避免 ("ab","c") 与 ("a","bc") 同值。
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in transcript
        .as_bytes()
        .iter()
        .chain(b"\0".iter())
        .chain(content.as_bytes())
    {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn detail_from_row(row: &Row<'_>) -> rusqlite::Result<NoteDetail> {
    let content: String = row.get(3)?;
    let transcript: Option<String> = row.get(4)?;
    let summary: Option<String> = row.get(5)?;
    let summary_source_hash: Option<String> = row.get(8)?;
    // 有摘要才谈「过期」：空态是「还没生成」，不是「过期了」。
    let has_summary = summary
        .as_deref()
        .is_some_and(|text| !text.trim().is_empty());
    let current_hash = source_hash(transcript.as_deref().unwrap_or(""), &content);
    let summary_stale =
        has_summary && summary_source_hash.as_deref() != Some(current_hash.as_str());
    Ok(NoteDetail {
        id: row.get(0)?,
        folder_id: row.get(1)?,
        title: row.get(2)?,
        content,
        transcript,
        summary,
        summary_model: row.get(6)?,
        summary_generated_at: row.get(7)?,
        summary_source_hash,
        summary_stale,
        kind: row.get(9)?,
        duration_sec: row.get(10)?,
        history_id: row.get(11)?,
        created_at: row.get(12)?,
        updated_at: row.get(13)?,
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

/// 写回 AI 摘要，连同生成用的模型与素材指纹。
///
/// 刻意**不动 `updated_at`**：摘要是对已有内容的派生结果，改排序时间会让笔记
/// 无端跳到列表最前；摘要自己的时间记在 `summary_generated_at`。
fn set_note_summary_with_connection(
    connection: &Connection,
    id: i64,
    summary: &str,
    model: &str,
    source_hash: &str,
    generated_at: i64,
) -> Result<Option<NoteDetail>, String> {
    let summary = summary.trim();
    if summary.is_empty() {
        return Err("摘要内容为空".into());
    }
    let changed = connection
        .execute(
            "UPDATE notes SET summary = ?2, summary_model = ?3, summary_generated_at = ?4, \
             summary_source_hash = ?5 WHERE id = ?1",
            params![id, summary, model, generated_at, source_hash],
        )
        .map_err(|error| format!("保存摘要失败: {error}"))?;
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

/// 删笔记，返回它所有段的音频文件名（外层 `None` = 笔记不存在）。
///
/// 段表虽然带 `ON DELETE CASCADE`，这里仍显式先删段：一是要先取到文件名交给调用方清盘，
/// 二是删除行为不依赖编译期的 `SQLITE_DEFAULT_FOREIGN_KEYS` 默认值（本项目 `bundled`
/// 构建默认是开的，但不该让正确性押在这个隐式事实上）。
fn delete_note_with_connection(
    connection: &Connection,
    id: i64,
) -> Result<Option<Vec<String>>, String> {
    let exists: Option<i64> = connection
        .query_row("SELECT id FROM notes WHERE id = ?1", params![id], |row| {
            row.get(0)
        })
        .optional()
        .map_err(|error| format!("读取待删除笔记失败: {error}"))?;
    if exists.is_none() {
        return Ok(None);
    }
    let mut statement = connection
        .prepare("SELECT audio_file FROM note_recordings WHERE note_id = ?1")
        .map_err(|error| format!("查询笔记录音失败: {error}"))?;
    let rows = statement
        .query_map(params![id], |row| row.get::<_, String>(0))
        .map_err(|error| format!("读取笔记录音失败: {error}"))?;
    let mut files = Vec::new();
    for row in rows {
        files.push(row.map_err(|error| format!("解析笔记录音失败: {error}"))?);
    }
    connection
        .execute(
            "DELETE FROM note_recordings WHERE note_id = ?1",
            params![id],
        )
        .map_err(|error| format!("删除笔记录音引用失败: {error}"))?;
    connection
        .execute("DELETE FROM notes WHERE id = ?1", params![id])
        .map_err(|error| format!("删除笔记失败: {error}"))?;
    Ok(Some(files))
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

/// 按历史记录反查笔记 id。
///
/// 两条路径都要走，缺一条就有一半的历史行点不动：
/// - 「历史 → 存为笔记」的笔记把 `history_id` 记在 `notes` 上；
/// - 会议录音写的历史行归属于某个**录音段**，`notes.history_id` 是空的，
///   只能从 `note_recordings.history_id` 反查（多段笔记任一段命中都算）。
fn find_note_id_by_history_with_connection(
    connection: &Connection,
    history_id: i64,
) -> Result<Option<i64>, String> {
    let direct = connection
        .query_row(
            "SELECT id FROM notes WHERE history_id = ?1 ORDER BY id LIMIT 1",
            params![history_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(|error| format!("反查笔记失败: {error}"))?;
    if direct.is_some() {
        return Ok(direct);
    }
    connection
        .query_row(
            "SELECT note_id FROM note_recordings WHERE history_id = ?1 ORDER BY id LIMIT 1",
            params![history_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(|error| format!("反查笔记失败: {error}"))
}

/// 批量版反查：历史列表一次带上整页 id，返回「历史行 → 笔记」的映射。
///
/// 逐条反查会让每张卡片发一次 IPC、开一次连接，一页 50 条就是 50 次往返；这里一次
/// 查完。判定顺序与单条版一致：先看 `notes.history_id`（存为笔记），再看
/// `note_recordings.history_id`（会议录音的段）。
fn find_note_ids_by_history_with_connection(
    connection: &Connection,
    history_ids: &[i64],
) -> Result<Vec<(i64, i64)>, String> {
    let mut links: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
    // SQLite 一条语句的变量上限是 999；一页 50 条远不到，但调用方可能一次传更多。
    for chunk in history_ids.chunks(500) {
        if chunk.is_empty() {
            continue;
        }
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!("SELECT history_id, id FROM notes WHERE history_id IN ({placeholders})");
        let mut statement = connection
            .prepare(&sql)
            .map_err(|error| format!("准备反查笔记失败: {error}"))?;
        let rows = statement
            .query_map(params_from_iter(chunk.iter()), |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(|error| format!("反查笔记失败: {error}"))?;
        for row in rows {
            let (history_id, note_id) =
                row.map_err(|error| format!("解析反查结果失败: {error}"))?;
            links.insert(history_id, note_id);
        }
    }

    // 只有前者没命中的才看段表：两处都有时优先「存为笔记」的那条。
    let missing: Vec<i64> = history_ids
        .iter()
        .copied()
        .filter(|id| !links.contains_key(id))
        .collect();
    for chunk in missing.chunks(500) {
        if chunk.is_empty() {
            continue;
        }
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "SELECT history_id, note_id FROM note_recordings \
             WHERE history_id IN ({placeholders}) ORDER BY id"
        );
        let mut statement = connection
            .prepare(&sql)
            .map_err(|error| format!("准备反查笔记录音段失败: {error}"))?;
        let rows = statement
            .query_map(params_from_iter(chunk.iter()), |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(|error| format!("反查笔记录音段失败: {error}"))?;
        for row in rows {
            let (history_id, note_id) =
                row.map_err(|error| format!("解析反查结果失败: {error}"))?;
            links.entry(history_id).or_insert(note_id);
        }
    }

    // 顺序跟调用方传进来的 id 一致，前端合并结果时不用再排。
    Ok(history_ids
        .iter()
        .filter_map(|id| links.get(id).map(|note_id| (*id, *note_id)))
        .collect())
}

fn recording_from_row(row: &Row<'_>) -> rusqlite::Result<NoteRecording> {
    Ok(NoteRecording {
        id: row.get(0)?,
        note_id: row.get(1)?,
        audio_file: row.get(2)?,
        duration_sec: row.get(3)?,
        transcript: row.get(4)?,
        history_id: row.get(5)?,
        sort_order: row.get(6)?,
        created_at: row.get(7)?,
    })
}

fn list_recordings_with_connection(
    connection: &Connection,
    note_id: i64,
) -> Result<Vec<NoteRecording>, String> {
    let mut statement = connection
        .prepare(
            "SELECT id, note_id, audio_file, duration_sec, transcript, history_id, sort_order, created_at \
             FROM note_recordings WHERE note_id = ?1 ORDER BY sort_order ASC, id ASC",
        )
        .map_err(|error| format!("准备查询笔记录音失败: {error}"))?;
    let rows = statement
        .query_map(params![note_id], recording_from_row)
        .map_err(|error| format!("查询笔记录音失败: {error}"))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| format!("解析笔记录音失败: {error}"))
}

/// 段转录变化后重算笔记的总时长与聚合转录。
///
/// 没有任何段时（`history_id` 转存来的笔记）保持原样：那种笔记的转录来自历史
/// 记录，音频不在笔记里，不属于段表管辖。
fn recompute_note_from_recordings_with_connection(
    connection: &Connection,
    note_id: i64,
) -> Result<(), String> {
    let mut statement = connection
        .prepare(
            "SELECT duration_sec, transcript FROM note_recordings \
             WHERE note_id = ?1 ORDER BY sort_order ASC, id ASC",
        )
        .map_err(|error| format!("准备重算笔记转录失败: {error}"))?;
    let rows = statement
        .query_map(params![note_id], |row| {
            Ok((
                row.get::<_, Option<f64>>(0)?,
                row.get::<_, Option<String>>(1)?,
            ))
        })
        .map_err(|error| format!("重算笔记转录失败: {error}"))?;
    let mut total = 0.0;
    let mut has_duration = false;
    let mut texts: Vec<String> = Vec::new();
    for row in rows {
        let (duration, transcript) = row.map_err(|error| format!("解析笔记录音失败: {error}"))?;
        if let Some(seconds) = duration {
            total += seconds;
            has_duration = true;
        }
        if let Some(text) = transcript {
            if !text.trim().is_empty() {
                texts.push(text.trim().to_string());
            }
        }
    }
    if !has_duration && texts.is_empty() {
        // 可能是「有段但都还没转写」：总时长仍然要算，只在完全没有段时才跳过。
        let count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM note_recordings WHERE note_id = ?1",
                params![note_id],
                |row| row.get(0),
            )
            .map_err(|error| format!("统计笔记录音失败: {error}"))?;
        if count == 0 {
            return Ok(());
        }
    }
    let joined = if texts.is_empty() {
        None
    } else {
        Some(texts.join("\n\n"))
    };
    let duration = if has_duration { Some(total) } else { None };
    connection
        .execute(
            "UPDATE notes SET transcript = ?2, duration_sec = ?3, updated_at = ?4 WHERE id = ?1",
            params![note_id, joined, duration, now_millis()],
        )
        .map_err(|error| format!("保存笔记聚合转录失败: {error}"))?;
    Ok(())
}

/// 停止录音后新增一段。不覆盖任何旧段。
fn add_recording_with_connection(
    connection: &Connection,
    note_id: i64,
    audio_file: &str,
    duration_sec: Option<f64>,
) -> Result<Option<NoteRecording>, String> {
    let exists: Option<i64> = connection
        .query_row(
            "SELECT id FROM notes WHERE id = ?1",
            params![note_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| format!("读取笔记失败: {error}"))?;
    if exists.is_none() {
        return Ok(None);
    }
    let next_order: i64 = connection
        .query_row(
            "SELECT COALESCE(MAX(sort_order), -1) + 1 FROM note_recordings WHERE note_id = ?1",
            params![note_id],
            |row| row.get(0),
        )
        .map_err(|error| format!("计算录音序号失败: {error}"))?;
    connection
        .execute(
            "INSERT INTO note_recordings \
                (note_id, audio_file, duration_sec, sort_order, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![note_id, audio_file, duration_sec, next_order, now_millis()],
        )
        .map_err(|error| format!("保存笔记录音失败: {error}"))?;
    let id = connection.last_insert_rowid();
    recompute_note_from_recordings_with_connection(connection, note_id)?;
    Ok(Some(
        connection
            .query_row(
                "SELECT id, note_id, audio_file, duration_sec, transcript, history_id, sort_order, created_at \
                 FROM note_recordings WHERE id = ?1",
                params![id],
                recording_from_row,
            )
            .map_err(|error| format!("读取新增笔记录音失败: {error}"))?,
    ))
}

fn get_recording_with_connection(
    connection: &Connection,
    id: i64,
) -> Result<Option<NoteRecording>, String> {
    connection
        .query_row(
            "SELECT id, note_id, audio_file, duration_sec, transcript, history_id, sort_order, created_at \
             FROM note_recordings WHERE id = ?1",
            params![id],
            recording_from_row,
        )
        .optional()
        .map_err(|error| format!("读取笔记录音失败: {error}"))
}

/// 删除一段录音，返回 `(note_id, audio_file)` 交给调用方清文件。
fn delete_recording_with_connection(
    connection: &Connection,
    id: i64,
) -> Result<Option<(i64, String)>, String> {
    let Some(recording) = get_recording_with_connection(connection, id)? else {
        return Ok(None);
    };
    connection
        .execute("DELETE FROM note_recordings WHERE id = ?1", params![id])
        .map_err(|error| format!("删除笔记录音失败: {error}"))?;
    recompute_note_from_recordings_with_connection(connection, recording.note_id)?;
    Ok(Some((recording.note_id, recording.audio_file)))
}

pub(crate) fn set_recording_transcript_with_connection(
    connection: &Connection,
    id: i64,
    transcript: &str,
    history_id: Option<i64>,
) -> Result<Option<NoteRecording>, String> {
    let changed = connection
        .execute(
            "UPDATE note_recordings SET transcript = ?2, \
                history_id = COALESCE(?3, history_id) WHERE id = ?1",
            params![id, transcript, history_id],
        )
        .map_err(|error| format!("保存分段转录失败: {error}"))?;
    if changed == 0 {
        return Ok(None);
    }
    let Some(recording) = get_recording_with_connection(connection, id)? else {
        return Ok(None);
    };
    recompute_note_from_recordings_with_connection(connection, recording.note_id)?;
    Ok(Some(recording))
}

// ---------------------------------------------------------------------------
// 对外接口
// ---------------------------------------------------------------------------

pub async fn list_folders() -> Result<Vec<NoteFolder>, String> {
    initialize().await?;
    tokio::task::spawn_blocking(|| {
        let connection = db::open()?;
        list_folders_with_connection(&connection)
    })
    .await
    .map_err(|error| format!("查询笔记目录任务失败: {error}"))?
}

pub async fn create_folder(name: &str) -> Result<NoteFolder, String> {
    initialize().await?;
    let name = name.to_string();
    tokio::task::spawn_blocking(move || {
        let connection = db::open()?;
        insert_folder_with_connection(&connection, &name)
    })
    .await
    .map_err(|error| format!("创建笔记目录任务失败: {error}"))?
}

pub async fn rename_folder(id: i64, name: &str) -> Result<NoteFolder, String> {
    initialize().await?;
    let name = name.to_string();
    tokio::task::spawn_blocking(move || {
        let connection = db::open()?;
        rename_folder_with_connection(&connection, id, &name)
    })
    .await
    .map_err(|error| format!("重命名笔记目录任务失败: {error}"))?
}

pub async fn delete_folder(id: i64) -> Result<bool, String> {
    initialize().await?;
    tokio::task::spawn_blocking(move || {
        let connection = db::open()?;
        delete_folder_with_connection(&connection, id)
    })
    .await
    .map_err(|error| format!("删除笔记目录任务失败: {error}"))?
}

pub async fn reorder_folders(ids: Vec<i64>) -> Result<Vec<NoteFolder>, String> {
    initialize().await?;
    tokio::task::spawn_blocking(move || {
        let connection = db::open()?;
        reorder_folders_with_connection(&connection, &ids)?;
        list_folders_with_connection(&connection)
    })
    .await
    .map_err(|error| format!("排序笔记目录任务失败: {error}"))?
}

pub async fn list_notes(query: NoteQuery) -> Result<NotePage, String> {
    initialize().await?;
    tokio::task::spawn_blocking(move || {
        let connection = db::open()?;
        list_notes_with_connection(&connection, &query)
    })
    .await
    .map_err(|error| format!("查询笔记任务失败: {error}"))?
}

pub async fn get_note(id: i64) -> Result<Option<NoteDetail>, String> {
    initialize().await?;
    tokio::task::spawn_blocking(move || {
        let connection = db::open()?;
        get_note_with_connection(&connection, id)
    })
    .await
    .map_err(|error| format!("读取笔记任务失败: {error}"))?
}

/// 历史记录 → 笔记 id；`None` 表示这条历史行对应的笔记已经被删。
pub async fn find_note_id_by_history(history_id: i64) -> Result<Option<i64>, String> {
    initialize().await?;
    tokio::task::spawn_blocking(move || {
        let connection = db::open()?;
        find_note_id_by_history_with_connection(&connection, history_id)
    })
    .await
    .map_err(|error| format!("反查笔记任务失败: {error}"))?
}

/// 批量反查历史行对应的笔记 id，供历史列表一次性补上「打开笔记」入口。
///
/// 返回 `Vec<(history_id, note_id)>`，只包含确实有笔记的行；顺序与入参一致。
pub async fn find_note_ids_by_history(history_ids: Vec<i64>) -> Result<Vec<(i64, i64)>, String> {
    initialize().await?;
    tokio::task::spawn_blocking(move || {
        let connection = db::open()?;
        find_note_ids_by_history_with_connection(&connection, &history_ids)
    })
    .await
    .map_err(|error| format!("反查笔记任务失败: {error}"))?
}

/// 所有被笔记引用的音频文件名。
///
/// 只给启动清理用：录音目录里不在这份集合中的文件就是没人认领的残留。
pub async fn audio_file_names() -> Result<std::collections::HashSet<String>, String> {
    initialize().await?;
    tokio::task::spawn_blocking(move || {
        let connection = db::open()?;
        audio_file_names_with_connection(&connection)
    })
    .await
    .map_err(|error| format!("查询笔记录音引用任务失败: {error}"))?
}

fn audio_file_names_with_connection(
    connection: &Connection,
) -> Result<std::collections::HashSet<String>, String> {
    let mut statement = connection
        .prepare("SELECT audio_file FROM note_recordings")
        .map_err(|error| format!("准备查询笔记录音引用失败: {error}"))?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| format!("查询笔记录音引用失败: {error}"))?;
    let mut names = std::collections::HashSet::new();
    for row in rows {
        names.insert(row.map_err(|error| format!("解析笔记录音引用失败: {error}"))?);
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
        let connection = db::open()?;
        insert_note_with_connection(&connection, folder_id, &title, &kind)
    })
    .await
    .map_err(|error| format!("创建笔记任务失败: {error}"))?
}

pub async fn update_note(id: i64, patch: NotePatch) -> Result<Option<NoteDetail>, String> {
    initialize().await?;
    tokio::task::spawn_blocking(move || {
        let connection = db::open()?;
        update_note_with_connection(&connection, id, &patch)
    })
    .await
    .map_err(|error| format!("保存笔记任务失败: {error}"))?
}

pub async fn move_note(id: i64, folder_id: Option<i64>) -> Result<Option<NoteDetail>, String> {
    initialize().await?;
    tokio::task::spawn_blocking(move || {
        let connection = db::open()?;
        move_note_with_connection(&connection, id, folder_id)
    })
    .await
    .map_err(|error| format!("移动笔记任务失败: {error}"))?
}

/// 写回 AI 摘要（异步包装）。生成失败的调用方不应走到这里：旧摘要在失败时保留。
pub async fn set_note_summary(
    id: i64,
    summary: &str,
    model: &str,
    source_hash: &str,
    generated_at: i64,
) -> Result<Option<NoteDetail>, String> {
    initialize().await?;
    let summary = summary.to_string();
    let model = model.to_string();
    let source_hash = source_hash.to_string();
    tokio::task::spawn_blocking(move || {
        let connection = db::open()?;
        set_note_summary_with_connection(
            &connection,
            id,
            &summary,
            &model,
            &source_hash,
            generated_at,
        )
    })
    .await
    .map_err(|error| format!("保存摘要任务失败: {error}"))?
}

/// 删除笔记，返回它所有段的音频文件名（外层 `None` = 笔记不存在）。
///
/// 音频文件的实际清理由录音模块负责，这里只把引用交回去。
pub async fn delete_note(id: i64) -> Result<Option<Vec<String>>, String> {
    initialize().await?;
    tokio::task::spawn_blocking(move || {
        let connection = db::open()?;
        delete_note_with_connection(&connection, id)
    })
    .await
    .map_err(|error| format!("删除笔记任务失败: {error}"))?
}

/// 停止录音后登记一段。每次 start/stop 都是新段，不覆盖旧段。
pub async fn add_note_recording(
    note_id: i64,
    audio_file: &str,
    duration_sec: Option<f64>,
) -> Result<Option<NoteRecording>, String> {
    initialize().await?;
    let audio_file = audio_file.to_string();
    tokio::task::spawn_blocking(move || {
        let connection = db::open()?;
        add_recording_with_connection(&connection, note_id, &audio_file, duration_sec)
    })
    .await
    .map_err(|error| format!("保存笔记录音任务失败: {error}"))?
}

pub async fn list_note_recordings(note_id: i64) -> Result<Vec<NoteRecording>, String> {
    initialize().await?;
    tokio::task::spawn_blocking(move || {
        let connection = db::open()?;
        list_recordings_with_connection(&connection, note_id)
    })
    .await
    .map_err(|error| format!("查询笔记录音任务失败: {error}"))?
}

pub async fn get_note_recording(id: i64) -> Result<Option<NoteRecording>, String> {
    initialize().await?;
    tokio::task::spawn_blocking(move || {
        let connection = db::open()?;
        get_recording_with_connection(&connection, id)
    })
    .await
    .map_err(|error| format!("读取笔记录音任务失败: {error}"))?
}

/// 删除一段，返回 `(note_id, audio_file)` 供调用方清理文件。
pub async fn delete_note_recording(id: i64) -> Result<Option<(i64, String)>, String> {
    initialize().await?;
    tokio::task::spawn_blocking(move || {
        let connection = db::open()?;
        delete_recording_with_connection(&connection, id)
    })
    .await
    .map_err(|error| format!("删除笔记录音任务失败: {error}"))?
}

/// 带说话人分段的转写落库：历史行、transcript、分段三者同一个事务。
///
/// 同事务的理由：转写期间笔记被删的话，历史行不能留下来当孤儿。
/// `reuse_history` 有值时**就地更新**那条历史（重新转写不堆重复记录），没有才新插。
/// 保留期清理放在提交之后：它自己要开写事务，塞进本事务会嵌套。
///
/// `segments` 为空是正常输入（没做分离、或者这次降级了），此时会把该段旧分段
/// 一起清掉——重转后模型换了人，留着旧分段比没有更糟。
pub async fn save_recording_transcription_with_segments(
    recording_id: i64,
    draft: history_service::HistoryDraft,
    reuse_history: Option<i64>,
    retention_days: u32,
    segments: Vec<SpeakerSegment>,
) -> Result<i64, String> {
    initialize().await?;
    tokio::task::spawn_blocking(move || {
        let mut connection = db::open()?;
        let history_id = save_recording_transcription_and_segments_with_connection(
            &mut connection,
            recording_id,
            &draft,
            reuse_history,
            &segments,
        )?;
        if let Err(error) =
            history_service::cleanup_expired_with_connection(&mut connection, retention_days)
        {
            log::warn!("笔记转录已保存，但历史保留期清理失败，将在下次启动重试: {error}");
        }
        Ok(history_id)
    })
    .await
    .map_err(|error| format!("保存笔记转录任务失败: {error}"))?
}

/// 读一篇录音的分段，按 `sort_order` 排列。没有分段（未分离）时返回空表。
pub async fn list_recording_segments(recording_id: i64) -> Result<Vec<SpeakerSegment>, String> {
    initialize().await?;
    tokio::task::spawn_blocking(move || {
        let connection = db::open()?;
        list_recording_segments_with_connection(&connection, recording_id)
    })
    .await
    .map_err(|error| format!("读取录音分段任务失败: {error}"))?
}

fn list_recording_segments_with_connection(
    connection: &Connection,
    recording_id: i64,
) -> Result<Vec<SpeakerSegment>, String> {
    let mut statement = connection
        .prepare(
            "SELECT speaker_id, start_ms, end_ms, text, overlap
             FROM note_recording_segments
             WHERE recording_id = ?1
             ORDER BY sort_order ASC, id ASC",
        )
        .map_err(|error| format!("准备读取录音分段失败: {error}"))?;
    let rows = statement
        .query_map([recording_id], |row| {
            Ok(SpeakerSegment {
                speaker_id: row.get(0)?,
                start_ms: row.get(1)?,
                end_ms: row.get(2)?,
                text: row.get(3)?,
                overlap: row.get::<_, i64>(4)? != 0,
            })
        })
        .map_err(|error| format!("读取录音分段失败: {error}"))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| format!("解析录音分段失败: {error}"))
}

fn write_history_and_transcript(
    transaction: &Transaction<'_>,
    recording_id: i64,
    draft: &history_service::HistoryDraft,
    reuse_history: Option<i64>,
) -> Result<i64, String> {
    let history_id = match reuse_history {
        Some(existing) => {
            let updated = history_service::update_transcription_result_with_connection(
                transaction,
                existing,
                &history_service::TranscriptionUpdate {
                    text: &draft.text,
                    engine: &draft.engine,
                    language: draft.language.clone(),
                    asr_ms: draft.asr_ms,
                    total_ms: draft.total_ms,
                    duration_sec: draft.duration_sec,
                },
            )?;
            // 旧行在「读出段」到「写回结果」之间被保留期清掉了：补插一条，
            // 而不是拿着一个已消失的 id 去写外键。
            if updated {
                existing
            } else {
                history_service::insert_with_connection(transaction, draft)?
            }
        }
        None => history_service::insert_with_connection(transaction, draft)?,
    };
    // 段被删掉了就整段回滚：历史行不能留下来当孤儿。
    let written = set_recording_transcript_with_connection(
        transaction,
        recording_id,
        &draft.text,
        Some(history_id),
    )?
    .is_some();
    if !written {
        return Err("这篇笔记已被删除".to_string());
    }
    Ok(history_id)
}

fn replace_recording_segments_with_connection(
    transaction: &Transaction<'_>,
    recording_id: i64,
    segments: &[SpeakerSegment],
) -> Result<(), String> {
    // 重新转写走同一个落库入口，先清旧段才不会越转越多。
    transaction
        .execute(
            "DELETE FROM note_recording_segments WHERE recording_id = ?1",
            [recording_id],
        )
        .map_err(|error| format!("清理旧的分段记录失败: {error}"))?;
    if segments.is_empty() {
        return Ok(());
    }
    let created_at = now_millis();
    let mut statement = transaction
        .prepare(
            "INSERT INTO note_recording_segments
                (recording_id, speaker_id, start_ms, end_ms, text, overlap, sort_order, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )
        .map_err(|error| format!("准备写入分段记录失败: {error}"))?;
    for (index, segment) in segments.iter().enumerate() {
        statement
            .execute(params![
                recording_id,
                segment.speaker_id,
                segment.start_ms,
                segment.end_ms,
                segment.text,
                if segment.overlap { 1i64 } else { 0i64 },
                index as i64,
                created_at,
            ])
            .map_err(|error| format!("写入分段记录失败: {error}"))?;
    }
    Ok(())
}

fn save_recording_transcription_and_segments_with_connection(
    connection: &mut Connection,
    recording_id: i64,
    draft: &history_service::HistoryDraft,
    reuse_history: Option<i64>,
    segments: &[SpeakerSegment],
) -> Result<i64, String> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| format!("开始转写落库事务失败: {error}"))?;
    let history_id =
        write_history_and_transcript(&transaction, recording_id, draft, reuse_history)?;
    replace_recording_segments_with_connection(&transaction, recording_id, segments)?;
    transaction
        .commit()
        .map_err(|error| format!("提交转写落库事务失败: {error}"))?;
    Ok(history_id)
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

/// 笔记标题取正文首个非空行的前 40 字：历史记录没有标题字段，只能从转录里摘。
fn derive_title(text: &str) -> String {
    let first = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let stripped = first.trim_start_matches(['#', '>', '-', '*', '+', ' ']);
    stripped.chars().take(40).collect()
}

/// 「历史记录 → 笔记」：读历史、查重、建笔记、写转录在同一个事务里。
///
/// 同事务的理由：历史行是笔记的引用来源，笔记是历史文本的落地。保留期 GC 插在
/// 读与写之间时，笔记会带着一个已消失的 `history_id` 落库，或者反过来在历史里
/// 留下没人认领的行。
pub async fn create_note_from_history(
    history_id: i64,
    folder_id: Option<i64>,
) -> Result<NoteDetail, String> {
    initialize().await?;
    tokio::task::spawn_blocking(move || {
        let mut connection = db::open()?;
        create_note_from_history_with_connection(&mut connection, history_id, folder_id)
    })
    .await
    .map_err(|error| format!("保存笔记任务失败: {error}"))?
}

fn create_note_from_history_with_connection(
    connection: &mut Connection,
    history_id: i64,
    folder_id: Option<i64>,
) -> Result<NoteDetail, String> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| format!("开始存为笔记事务失败: {error}"))?;
    let stored = history_service::get_with_connection(&transaction, history_id)?
        .ok_or_else(|| "历史记录不存在，可能已被删除".to_string())?;
    let transcript = stored.record.text.trim().to_string();
    if transcript.is_empty() {
        return Err("这条记录没有转写文本，无法存为笔记".into());
    }
    let title = derive_title(&transcript);
    let (note, _) = create_note_from_transcript_with_connection(
        &transaction,
        folder_id,
        &title,
        "import",
        &transcript,
        stored.record.duration_sec,
        history_id,
    )?;
    transaction
        .commit()
        .map_err(|error| format!("提交存为笔记事务失败: {error}"))?;
    Ok(note)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory_connection() -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        ensure_schema(&connection).unwrap();
        seed_history(&connection, &[7, 42, 99]);
        connection
    }

    /// `history_id` 现在是真外键，测试里引用的历史行必须真的存在。
    fn seed_history(connection: &Connection, ids: &[i64]) {
        for id in ids {
            connection
                .execute(
                    "INSERT INTO transcription_history \
                     (id, session_id, created_at, updated_at, mode, status, text, original_text, engine) \
                     VALUES (?1, 1, 0, 0, 'import', 'success', '转录', '转录', 'local')",
                    params![id],
                )
                .unwrap();
        }
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
    fn history_lookup_covers_imported_notes_and_meeting_segments() {
        let connection = memory_connection();

        // 路径一：「历史 → 存为笔记」，history_id 记在 notes 上。
        let (imported, _) = create_note_from_transcript_with_connection(
            &connection,
            None,
            "导入的笔记",
            "import",
            "转录内容",
            None,
            7,
        )
        .unwrap();
        assert_eq!(
            find_note_id_by_history_with_connection(&connection, 7).unwrap(),
            Some(imported.id)
        );

        // 路径二：会议录音，history_id 在段表上，notes.history_id 为空。
        let meeting = insert_note_with_connection(&connection, None, "周会", "meeting").unwrap();
        assert!(meeting.history_id.is_none());
        let segment = add_recording_with_connection(&connection, meeting.id, "a.wav", None)
            .unwrap()
            .unwrap();
        set_recording_transcript_with_connection(&connection, segment.id, "第一段", Some(99))
            .unwrap();
        assert_eq!(
            find_note_id_by_history_with_connection(&connection, 99).unwrap(),
            Some(meeting.id)
        );

        // 笔记已删、历史行还在：返回 None 而不是报错，前端才能提示「笔记已删除」。
        assert_eq!(
            find_note_id_by_history_with_connection(&connection, 12345).unwrap(),
            None
        );
    }

    #[test]
    fn history_batch_lookup_matches_single_lookup_and_keeps_order() {
        let connection = memory_connection();
        let (imported, _) = create_note_from_transcript_with_connection(
            &connection,
            None,
            "导入的笔记",
            "import",
            "转录内容",
            None,
            7,
        )
        .unwrap();
        let meeting = insert_note_with_connection(&connection, None, "周会", "meeting").unwrap();
        let segment = add_recording_with_connection(&connection, meeting.id, "a.wav", None)
            .unwrap()
            .unwrap();
        set_recording_transcript_with_connection(&connection, segment.id, "第一段", Some(99))
            .unwrap();

        // 顺序跟入参走，且只返回真正有笔记的行（12345 没有笔记）。
        assert_eq!(
            find_note_ids_by_history_with_connection(&connection, &[7, 12345, 99]).unwrap(),
            vec![(7, imported.id), (99, meeting.id)]
        );
        // 空入参不报错：历史列表可能一次都没渲染，前端也会无条件调一次。
        assert!(find_note_ids_by_history_with_connection(&connection, &[])
            .unwrap()
            .is_empty());
    }

    #[test]
    fn source_hash_changes_with_either_material_side() {
        assert_eq!(source_hash("转录", "正文"), source_hash("转录", "正文"));
        assert_ne!(
            source_hash("转录", "正文"),
            source_hash("转录", "改过的正文")
        );
        assert_ne!(
            source_hash("转录", "正文"),
            source_hash("改过的转录", "正文")
        );
    }

    #[test]
    fn source_hash_is_not_confused_by_field_boundaries() {
        // ("ab","c") 与 ("a","bc") 拼起来相同，指纹必须不同。
        assert_ne!(source_hash("ab", "c"), source_hash("a", "bc"));
    }

    #[test]
    fn summary_write_records_model_hash_and_keeps_updated_at() {
        let connection = memory_connection();
        let note = insert_note_with_connection(&connection, None, "周会", "meeting").unwrap();

        let hash = source_hash(note.transcript.as_deref().unwrap_or(""), &note.content);
        let updated =
            set_note_summary_with_connection(&connection, note.id, "  要点  ", "gpt-4o", &hash, 42)
                .unwrap()
                .unwrap();

        assert_eq!(updated.summary.as_deref(), Some("要点"));
        assert_eq!(updated.summary_model.as_deref(), Some("gpt-4o"));
        assert_eq!(updated.summary_generated_at, Some(42));
        assert_eq!(updated.summary_source_hash.as_deref(), Some(hash.as_str()));
        // 摘要是派生结果，不该把笔记顶到「最后编辑」列表最前。
        assert_eq!(updated.updated_at, note.updated_at);

        // 素材没变 → 不过期；改正文 → 旧摘要立刻标记为过期。
        assert!(!updated.summary_stale);
        let edited = update_note_with_connection(
            &connection,
            note.id,
            &NotePatch {
                title: None,
                content: Some("改过的正文".into()),
            },
        )
        .unwrap()
        .unwrap();
        assert!(edited.summary_stale);
    }

    #[test]
    fn summary_write_rejects_blank_and_reports_missing_note() {
        let connection = memory_connection();
        let note = insert_note_with_connection(&connection, None, "周会", "meeting").unwrap();

        assert!(
            set_note_summary_with_connection(&connection, note.id, "   ", "m", "h", 1).is_err()
        );
        assert!(
            set_note_summary_with_connection(&connection, note.id + 999, "要点", "m", "h", 1)
                .unwrap()
                .is_none()
        );
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
    fn delete_note_returns_all_audio_references_for_the_caller_to_clean() {
        let connection = memory_connection();
        let note = insert_note_with_connection(&connection, None, "会议", "meeting").unwrap();
        add_recording_with_connection(&connection, note.id, "1-123.wav", Some(30.0))
            .unwrap()
            .unwrap();
        add_recording_with_connection(&connection, note.id, "1-456.wav", Some(45.0))
            .unwrap()
            .unwrap();
        assert_eq!(
            delete_note_with_connection(&connection, note.id).unwrap(),
            Some(vec!["1-123.wav".to_string(), "1-456.wav".to_string()])
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
        let with_audio =
            insert_note_with_connection(&connection, None, "有录音", "meeting").unwrap();
        let without_audio =
            insert_note_with_connection(&connection, None, "无录音", "personal").unwrap();
        add_recording_with_connection(&connection, with_audio.id, "note-1-2.wav", Some(12.5))
            .unwrap();

        let names = audio_file_names_with_connection(&connection).unwrap();
        assert_eq!(names.len(), 1);
        assert!(names.contains("note-1-2.wav"));
        assert!(!names.iter().any(|name| name.is_empty()));

        // 删掉唯一的引用后集合必须为空，否则启动清理不敢删任何文件。
        delete_note_with_connection(&connection, with_audio.id).unwrap();
        assert!(audio_file_names_with_connection(&connection)
            .unwrap()
            .is_empty());
        assert!(get_note_with_connection(&connection, without_audio.id)
            .unwrap()
            .is_some());
    }

    #[test]
    fn recordings_are_appended_and_recomputed_into_the_note() {
        let connection = memory_connection();
        let note = insert_note_with_connection(&connection, None, "会议", "meeting").unwrap();
        let first = add_recording_with_connection(&connection, note.id, "a.wav", Some(61.5))
            .unwrap()
            .unwrap();
        let second = add_recording_with_connection(&connection, note.id, "b.wav", Some(30.0))
            .unwrap()
            .unwrap();
        assert_ne!(first.id, second.id);
        assert_eq!(first.sort_order, 0);
        assert_eq!(second.sort_order, 1);

        let recordings = list_recordings_with_connection(&connection, note.id).unwrap();
        assert_eq!(recordings.len(), 2);
        assert_eq!(recordings[0].audio_file, "a.wav");
        assert_eq!(recordings[1].audio_file, "b.wav");

        set_recording_transcript_with_connection(&connection, first.id, "第一段", None).unwrap();
        set_recording_transcript_with_connection(&connection, second.id, "第二段", Some(42))
            .unwrap();
        let updated = get_note_with_connection(&connection, note.id)
            .unwrap()
            .unwrap();
        assert_eq!(updated.transcript.as_deref(), Some("第一段\n\n第二段"));
        assert!((updated.duration_sec.unwrap() - 91.5).abs() < 1e-9);
        assert_eq!(
            list_recordings_with_connection(&connection, note.id).unwrap()[1].history_id,
            Some(42)
        );

        // 删一段只删该段引用，另一段与聚合转录都还在。
        assert_eq!(
            delete_recording_with_connection(&connection, second.id).unwrap(),
            Some((note.id, "b.wav".to_string()))
        );
        let updated = get_note_with_connection(&connection, note.id)
            .unwrap()
            .unwrap();
        assert_eq!(updated.transcript.as_deref(), Some("第一段"));
        assert!((updated.duration_sec.unwrap() - 61.5).abs() < 1e-9);
    }

    #[test]
    fn schema_creation_yields_the_final_shape_without_legacy_columns() {
        let connection = Connection::open_in_memory().unwrap();
        ensure_schema(&connection).unwrap();
        ensure_schema(&connection).expect("建表重复执行是幂等的");

        let columns: Vec<String> = connection
            .prepare("PRAGMA table_info('notes')")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(
            !columns.iter().any(|column| column == "audio_file"),
            "notes 不该有 audio_file 列，音频只归 note_recordings"
        );
        for expected in ["history_id", "transcript", "duration_sec", "summary"] {
            assert!(
                columns.iter().any(|column| column == expected),
                "建表后应有 {expected} 列，实际: {columns:?}"
            );
        }

        // 一条历史只能转出一篇笔记：唯一索引在。
        let indexes: Vec<String> = connection
            .prepare("SELECT name FROM sqlite_master WHERE type='index' AND tbl_name='notes'")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(
            indexes.iter().any(|name| name == "idx_notes_history"),
            "应有 notes(history_id) 唯一索引，实际: {indexes:?}"
        );
    }

    /// 历史行被淘汰只应断开笔记侧的引用。
    #[test]
    fn deleting_history_row_only_clears_the_note_reference() {
        let connection = Connection::open_in_memory().unwrap();
        db::configure(&connection).unwrap();
        ensure_schema(&connection).unwrap();
        connection
            .execute(
                "INSERT INTO transcription_history \
                 (id, session_id, created_at, updated_at, mode, status, text, original_text, engine) \
                 VALUES (7, 1, 0, 0, 'import', 'success', '转录文本', '转录文本', 'local')",
                [],
            )
            .unwrap();

        let (note, created) = create_note_from_transcript_with_connection(
            &connection,
            None,
            "来源",
            "import",
            "转录文本",
            None,
            7,
        )
        .unwrap();
        assert!(created);
        assert_eq!(note.history_id, Some(7));

        connection
            .execute("DELETE FROM transcription_history WHERE id = 7", [])
            .unwrap();

        let after = get_note_with_connection(&connection, note.id)
            .unwrap()
            .unwrap();
        assert_eq!(after.history_id, None, "历史行删除后引用应被置空");
        assert_eq!(
            after.transcript.as_deref(),
            Some("转录文本"),
            "笔记正文不受影响"
        );
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
            &connection,
            None,
            "周会",
            "import",
            "转录",
            None,
            7,
        )
        .unwrap();
        let (second, created_again) = create_note_from_transcript_with_connection(
            &connection,
            None,
            "周会",
            "import",
            "转录",
            None,
            7,
        )
        .unwrap();

        assert!(created);
        assert!(!created_again);
        assert_eq!(first.id, second.id);
        let page = list_notes_with_connection(&connection, &query(None, false, "")).unwrap();
        assert_eq!(page.total, 1);
    }

    /// 「历史 → 笔记」现在是一次事务：读历史、查重、建笔记、写转录一起成败。
    #[test]
    fn create_note_from_history_reads_and_writes_in_one_transaction() {
        let mut connection = memory_connection();
        // 这一行的正文决定标题与转录，所以单独造一条文本明确的记录。
        connection
            .execute(
                "INSERT INTO transcription_history \
                 (id, session_id, created_at, updated_at, mode, status, text, original_text, \
                  duration_sec, engine) \
                 VALUES (123, 1, 0, 0, 'import', 'success', '# 产品评审\n正文', '# 产品评审\n正文', \
                  90.0, 'local')",
                [],
            )
            .unwrap();

        let note = create_note_from_history_with_connection(&mut connection, 123, None).unwrap();
        assert_eq!(note.title, "产品评审");
        assert_eq!(note.transcript.as_deref(), Some("# 产品评审\n正文"));
        assert_eq!(note.duration_sec, Some(90.0));
        assert_eq!(note.history_id, Some(123));

        // 重复调用返回同一条，不产生第二篇。
        let again = create_note_from_history_with_connection(&mut connection, 123, None).unwrap();
        assert_eq!(again.id, note.id);

        // 历史不存在时既不建笔记也不报 SQL 错误。
        let error = create_note_from_history_with_connection(&mut connection, 999, None)
            .expect_err("missing history should fail");
        assert!(error.contains("不存在"));
    }

    /// 段已被删时，同一事务里刚插入的历史行必须一起回滚。
    #[test]
    fn save_recording_transcription_rolls_back_the_history_row() {
        let mut connection = memory_connection();
        let note = insert_note_with_connection(&connection, None, "会议", "meeting").unwrap();
        let recording = add_recording_with_connection(&connection, note.id, "a.wav", Some(10.0))
            .unwrap()
            .unwrap();

        let draft = |text: &str| history_service::HistoryDraft {
            session_id: 1,
            mode: "note".into(),
            workflow: "note".into(),
            status: "success".into(),
            text: text.to_string(),
            original_text: text.to_string(),
            source_text: None,
            duration_sec: Some(10.0),
            language: Some("zh".into()),
            engine: "local".into(),
            provider: None,
            model: None,
            app_process: None,
            app_window_title: None,
            app_rule_name: None,
            audio_file: None,
            asr_ms: Some(100),
            polish_ms: None,
            total_ms: Some(120),
            error: None,
            reprocessed_from_id: None,
        };

        // 正常路径：历史与段转录一起落库。
        let history_id = save_recording_transcription_with_connection(
            &mut connection,
            recording.id,
            draft("第一段"),
            None,
        )
        .unwrap();
        let stored = get_recording_with_connection(&connection, recording.id)
            .unwrap()
            .unwrap();
        assert_eq!(stored.transcript.as_deref(), Some("第一段"));
        assert_eq!(stored.history_id, Some(history_id));

        // 段不存在：历史行不能留下来。
        let before = count_history(&connection);
        let error = save_recording_transcription_with_connection(
            &mut connection,
            9999,
            draft("孤儿"),
            None,
        )
        .expect_err("missing recording should fail");
        assert!(error.contains("已被删除"));
        assert_eq!(count_history(&connection), before, "失败时不应多出历史行");
    }

    /// 重新转写沿用原历史行；原行已被保留期清掉时补插一条，不拿死 id 去写外键。
    #[test]
    fn save_recording_transcription_reuses_or_recreates_the_history_row() {
        let mut connection = memory_connection();
        let note = insert_note_with_connection(&connection, None, "会议", "meeting").unwrap();
        let recording = add_recording_with_connection(&connection, note.id, "a.wav", Some(10.0))
            .unwrap()
            .unwrap();
        let draft = |text: &str| history_service::HistoryDraft {
            session_id: 1,
            mode: "note".into(),
            workflow: "note".into(),
            status: "success".into(),
            text: text.to_string(),
            original_text: text.to_string(),
            source_text: None,
            duration_sec: Some(10.0),
            language: None,
            engine: "local".into(),
            provider: None,
            model: None,
            app_process: None,
            app_window_title: None,
            app_rule_name: None,
            audio_file: None,
            asr_ms: None,
            polish_ms: None,
            total_ms: None,
            error: None,
            reprocessed_from_id: None,
        };

        let first = save_recording_transcription_with_connection(
            &mut connection,
            recording.id,
            draft("初稿"),
            None,
        )
        .unwrap();
        let before = count_history(&connection);

        // 复用：行数不变，内容被覆盖。
        let reused = save_recording_transcription_with_connection(
            &mut connection,
            recording.id,
            draft("重转"),
            Some(first),
        )
        .unwrap();
        assert_eq!(reused, first);
        assert_eq!(count_history(&connection), before);
        let text: String = connection
            .query_row(
                "SELECT text FROM transcription_history WHERE id = ?1",
                params![first],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(text, "重转");

        // 原行已不在：补插一条，段指向新行，外键仍然成立。
        let recreated = save_recording_transcription_with_connection(
            &mut connection,
            recording.id,
            draft("补插"),
            Some(9999),
        )
        .unwrap();
        assert_ne!(recreated, first);
        let stored = get_recording_with_connection(&connection, recording.id)
            .unwrap()
            .unwrap();
        assert_eq!(stored.history_id, Some(recreated));
    }

    fn count_history(connection: &Connection) -> i64 {
        connection
            .query_row("SELECT COUNT(*) FROM transcription_history", [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    /// 无分段落库的便捷写法。
    ///
    /// 生产代码只有「带分段」一条落库路径（避免两条路径各写一遍历史行而慢慢分叉），
    /// 关心历史行的测试用这个薄封装把 `segments` 填成空表即可。
    fn save_recording_transcription_with_connection(
        connection: &mut Connection,
        recording_id: i64,
        draft: history_service::HistoryDraft,
        reuse_history: Option<i64>,
    ) -> Result<i64, String> {
        save_recording_transcription_and_segments_with_connection(
            connection,
            recording_id,
            &draft,
            reuse_history,
            &[],
        )
    }

    fn note_draft(text: &str) -> history_service::HistoryDraft {
        history_service::HistoryDraft {
            session_id: 1,
            mode: "note".into(),
            workflow: "note".into(),
            status: "success".into(),
            text: text.to_string(),
            original_text: text.to_string(),
            source_text: None,
            duration_sec: Some(60.0),
            language: Some("zh".into()),
            engine: "local".into(),
            provider: None,
            model: None,
            app_process: None,
            app_window_title: None,
            app_rule_name: None,
            audio_file: None,
            asr_ms: None,
            polish_ms: None,
            total_ms: None,
            error: None,
            reprocessed_from_id: None,
        }
    }

    fn speaker_segment(speaker_id: i64, start_ms: i64, end_ms: i64, text: &str) -> SpeakerSegment {
        SpeakerSegment {
            speaker_id,
            start_ms,
            end_ms,
            text: text.to_string(),
            overlap: false,
        }
    }

    /// 分段跟转录同一次落库；重转整段替换，降级成空分段时旧记录一起清掉。
    #[test]
    fn recording_segments_are_replaced_not_appended() {
        let mut connection = memory_connection();
        let note = insert_note_with_connection(&connection, None, "会议", "meeting").unwrap();
        let recording = add_recording_with_connection(&connection, note.id, "a.wav", Some(60.0))
            .unwrap()
            .unwrap();

        let first = save_recording_transcription_and_segments_with_connection(
            &mut connection,
            recording.id,
            &note_draft("甲说话。乙说话。"),
            None,
            &[
                speaker_segment(0, 0, 1_500, "甲说话。"),
                speaker_segment(1, 1_500, 4_000, "乙说话。"),
            ],
        )
        .unwrap();
        let stored = list_recording_segments_with_connection(&connection, recording.id).unwrap();
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0].text, "甲说话。");
        assert_eq!(stored[1].speaker_id, 1);

        // 重转：沿用原历史行，分段整段替换而不是追加。
        let second = save_recording_transcription_and_segments_with_connection(
            &mut connection,
            recording.id,
            &note_draft("丙说话。"),
            Some(first),
            &[speaker_segment(2, 0, 900, "丙说话。")],
        )
        .unwrap();
        assert_eq!(second, first, "重转应就地更新原来的历史行");
        let stored = list_recording_segments_with_connection(&connection, recording.id).unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].speaker_id, 2);

        // 降级重转（这次没分段）：旧分段必须一起清掉，否则界面上的人跟正文对不上。
        save_recording_transcription_and_segments_with_connection(
            &mut connection,
            recording.id,
            &note_draft("这次没分离"),
            Some(first),
            &[],
        )
        .unwrap();
        assert!(
            list_recording_segments_with_connection(&connection, recording.id)
                .unwrap()
                .is_empty()
        );
    }

    /// 分段按写入顺序读回，且不会串到同一篇笔记的另一段录音上。
    #[test]
    fn segments_are_scoped_to_their_own_recording() {
        let mut connection = memory_connection();
        let note = insert_note_with_connection(&connection, None, "会议", "meeting").unwrap();
        let first = add_recording_with_connection(&connection, note.id, "a.wav", Some(30.0))
            .unwrap()
            .unwrap();
        let second = add_recording_with_connection(&connection, note.id, "b.wav", Some(30.0))
            .unwrap()
            .unwrap();

        save_recording_transcription_and_segments_with_connection(
            &mut connection,
            first.id,
            &note_draft("甲"),
            None,
            &[
                speaker_segment(0, 0, 1_000, "先"),
                speaker_segment(1, 1_000, 2_000, "后"),
            ],
        )
        .unwrap();
        save_recording_transcription_and_segments_with_connection(
            &mut connection,
            second.id,
            &note_draft("乙"),
            None,
            &[speaker_segment(0, 0, 500, "另一段")],
        )
        .unwrap();

        // 读回顺序由 sort_order 决定（写入顺序），不依赖时间戳。
        let first_segments =
            list_recording_segments_with_connection(&connection, first.id).unwrap();
        assert_eq!(
            first_segments
                .iter()
                .map(|item| item.text.as_str())
                .collect::<Vec<_>>(),
            vec!["先", "后"]
        );
        let second_segments =
            list_recording_segments_with_connection(&connection, second.id).unwrap();
        assert_eq!(second_segments.len(), 1);
        assert_eq!(second_segments[0].text, "另一段");

        // 删段录音要把它的分段一起带走（外键 ON DELETE CASCADE）。
        delete_recording_with_connection(&connection, second.id)
            .unwrap()
            .expect("recording should exist");
        assert!(
            list_recording_segments_with_connection(&connection, second.id)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn note_title_comes_from_the_first_content_line() {
        assert_eq!(derive_title("周会纪要\n\n第二段"), "周会纪要");
        assert_eq!(derive_title("# 标题行\n正文"), "标题行");
        assert_eq!(derive_title("\n\n  第二行才有内容"), "第二行才有内容");
        assert_eq!(derive_title(""), "");
    }

    #[test]
    fn note_title_is_capped_without_splitting_characters() {
        let title = derive_title(&"会".repeat(60));
        assert_eq!(title.chars().count(), 40);
    }
}
