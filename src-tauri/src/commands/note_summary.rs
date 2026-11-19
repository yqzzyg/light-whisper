//! 笔记摘要命令：把转录压成结构化 markdown 纪要。
//!
//! 与转写的边界：
//!
//! - **可并行**：摘要走 LLM，转写走 ASR，占位分开（`note_summary` 与
//!   `note_transcription` 互不阻塞）。
//! - **失败不留脏数据**：只有成功拿到非空摘要才写库；失败或取消时旧摘要原样保留。
//! - **进度走事件**：`note-summary-status` 依次发 preparing / part / merging /
//!   done / failed / cancelled，前端按 `noteId` 过滤，切走页面也能看到结果。

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;
use tauri::Emitter;

use crate::services::{note_summary_service, notes_service, profile_service};
use crate::state::AppState;
use crate::utils::AppError;

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or_default()
}

fn emit_failure(app: &tauri::AppHandle, note_id: i64, status: &str, message: &str) {
    let _ = app.emit(
        note_summary_service::EVENT,
        json!({ "noteId": note_id, "status": status, "message": message }),
    );
}

/// 生成（或重新生成）一篇笔记的摘要，返回写库后的笔记。
///
/// 重新生成时不先清空旧摘要：生成失败或用户取消，用户至少还留着上一版。
#[tauri::command]
pub async fn generate_note_summary(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    note_id: i64,
) -> Result<notes_service::NoteDetail, AppError> {
    let note = notes_service::get_note(note_id)
        .await
        .map_err(AppError::Other)?
        .ok_or_else(|| AppError::Other("笔记不存在，可能已被删除".into()))?;

    let _guard = state.note_summary.try_begin(note_id).ok_or_else(|| {
        match state.note_summary.active_note() {
            Some(active) if active == note_id => {
                AppError::Other("这篇笔记正在生成摘要，请稍候".into())
            }
            _ => AppError::Other("已有另一篇笔记正在生成摘要，请等它完成再试".into()),
        }
    })?;

    let outcome = note_summary_service::generate(
        &app,
        state.inner(),
        &note,
        state.note_summary.cancel_flag(),
    )
    .await;

    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(message) if message == note_summary_service::CANCELLED => {
            // 事件里的文案只给人看；前端靠 `AppError::Cancelled` 的错误码判定，
            // 不比这段中文文案。
            emit_failure(&app, note_id, "cancelled", "已取消生成");
            return Err(AppError::Cancelled);
        }
        Err(message) => {
            emit_failure(&app, note_id, "failed", &message);
            return Err(AppError::Other(message));
        }
    };

    let detail = notes_service::set_note_summary(
        note_id,
        &outcome.summary,
        &outcome.model,
        &outcome.source_hash,
        now_millis(),
    )
    .await
    .map_err(AppError::Other)?
    .ok_or_else(|| AppError::Other("摘要已生成，但这篇笔记已被删除".into()))?;

    let _ = app.emit(
        note_summary_service::EVENT,
        json!({
            "noteId": note_id,
            "status": "done",
            "summary": outcome.summary,
            "model": outcome.model,
        }),
    );
    Ok(detail)
}

/// 请求取消正在跑的摘要生成。
///
/// 只能块间生效：一次 LLM 调用中途无法打断，正在飞的那次返回后才停下。
#[tauri::command]
pub fn cancel_note_summary(state: tauri::State<'_, AppState>) {
    state.note_summary.request_cancel();
}

/// 内置提示词：给前端的「填入默认 / 恢复默认」用。
///
/// 默认文案只在 Rust 常量里存一份，前端不另抄——否则两边改一处必然漂移。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NoteSummaryPromptDefaults {
    pub merge: String,
    pub part: String,
    /// 长度上限跟着默认值一起下发，前端不用自己抄一份常量。
    pub max_chars: usize,
}

#[tauri::command]
pub fn get_note_summary_prompt_defaults() -> NoteSummaryPromptDefaults {
    NoteSummaryPromptDefaults {
        merge: note_summary_service::DEFAULT_MERGE_PROMPT.to_string(),
        part: note_summary_service::DEFAULT_PART_PROMPT.to_string(),
        max_chars: note_summary_service::MAX_PROMPT_CHARS,
    }
}

/// 归一化用户填的提示词：空白视为「不配置」，超长直接拒。
fn normalize_prompt(prompt: Option<String>, label: &str) -> Result<Option<String>, String> {
    let Some(prompt) = prompt else {
        return Ok(None);
    };
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return Ok(None);
    }
    let chars = prompt.chars().count();
    if chars > note_summary_service::MAX_PROMPT_CHARS {
        return Err(format!(
            "{label}最多 {} 字符，当前 {chars} 字符",
            note_summary_service::MAX_PROMPT_CHARS
        ));
    }
    Ok(Some(prompt.to_string()))
}

/// 设置最终成稿提示词；传 `null` 或空白恢复内置默认。
#[tauri::command]
pub async fn set_note_summary_prompt(
    state: tauri::State<'_, AppState>,
    prompt: Option<String>,
) -> Result<(), String> {
    let prompt = normalize_prompt(prompt, "摘要提示词")?;
    profile_service::update_profile_and_schedule(state.inner(), |profile| {
        profile.note_summary_prompt = prompt;
    });
    Ok(())
}

/// 设置长会议分段提炼提示词；传 `null` 或空白恢复内置默认。
#[tauri::command]
pub async fn set_note_summary_part_prompt(
    state: tauri::State<'_, AppState>,
    prompt: Option<String>,
) -> Result<(), String> {
    let prompt = normalize_prompt(prompt, "分段提炼提示词")?;
    profile_service::update_profile_and_schedule(state.inner(), |profile| {
        profile.note_summary_part_prompt = prompt;
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{normalize_prompt, now_millis};
    use crate::services::note_summary_service::MAX_PROMPT_CHARS;

    #[test]
    fn summary_timestamp_looks_like_milliseconds() {
        // 2020-01-01 之后、2100 之前；只防「误用秒」这类低级错。
        let value = now_millis();
        assert!(value > 1_577_836_800_000, "unexpected timestamp: {value}");
        assert!(value < 4_102_444_800_000, "unexpected timestamp: {value}");
    }

    #[test]
    fn blank_prompt_means_use_builtin_default() {
        assert_eq!(normalize_prompt(None, "摘要提示词").unwrap(), None);
        assert_eq!(
            normalize_prompt(Some("  \n\t ".into()), "摘要提示词").unwrap(),
            None
        );
    }

    #[test]
    fn prompt_is_trimmed_before_saving() {
        assert_eq!(
            normalize_prompt(Some("  自定义要求  ".into()), "摘要提示词").unwrap(),
            Some("自定义要求".to_string())
        );
    }

    #[test]
    fn overlong_prompt_is_rejected() {
        let long = "字".repeat(MAX_PROMPT_CHARS + 1);
        let error = normalize_prompt(Some(long), "摘要提示词").unwrap_err();
        assert!(error.contains("最多"), "unexpected error: {error}");
        // 正好等于上限要放行，避免「卡在边界上」的迷惑行为。
        let at_limit = "字".repeat(MAX_PROMPT_CHARS);
        assert!(normalize_prompt(Some(at_limit), "摘要提示词").is_ok());
    }
}
