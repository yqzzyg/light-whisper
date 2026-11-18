//! 笔记转写命令：把已落盘的会议录音整段转成文字，再写回笔记。
//!
//! 与听写链路的边界：
//!
//! - **不复制音频**：直接把 `notes_audio/` 里的文件路径交给引擎，历史行
//!   `audio_file` 留空（音频归笔记，不归历史）。
//! - **幂等**：笔记已有转录就直接返回，重复点不会重跑 ASR、也不会多一条历史。
//! - **一次一条**：`AppState::note_transcription` 挡住连点造成的并发转写。
//! - **失败不落历史**：转写失败时笔记原样保留（录音还在，按钮回到可重试），
//!   不写半条空历史；失败原因由命令返回给前端就地展示。

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tauri::Emitter;

use crate::services::{history_service, notes_service, transcription_service};
use crate::state::AppState;
use crate::utils::{paths, AppError};

fn now_millis_u64() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u64::MAX as u128) as u64
}

/// 笔记转写写入历史用的草稿。
///
/// `audio_file` 恒为 `None`：录音文件由笔记引用管理，写进历史会让历史 GC
/// 与笔记录音目录互相越界（删历史顺手删掉笔记的唯一录音）。
fn history_draft_for_note(
    session_id: u64,
    text: &str,
    duration_sec: Option<f64>,
    language: Option<String>,
    engine: String,
    asr_ms: Option<u64>,
    total_ms: Option<u64>,
) -> history_service::HistoryDraft {
    history_service::HistoryDraft {
        session_id,
        mode: "note".into(),
        workflow: "note".into(),
        status: "success".into(),
        text: text.to_string(),
        original_text: text.to_string(),
        source_text: None,
        duration_sec,
        language,
        engine,
        provider: None,
        model: None,
        app_process: None,
        app_window_title: None,
        app_rule_name: None,
        audio_file: None,
        asr_ms,
        polish_ms: None,
        total_ms,
        error: None,
        reprocessed_from_id: None,
    }
}

/// 把笔记的会议录音整段转成文本。
///
/// 返回更新后的笔记（含转录与 `history_id`）。转写期间可以切走页面：
/// 命令本身不会被取消，前端回到笔记时重读即可看到结果。
#[tauri::command]
pub async fn transcribe_note_recording(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    note_id: i64,
) -> Result<notes_service::NoteDetail, AppError> {
    let note = notes_service::get_note(note_id)
        .await
        .map_err(AppError::Asr)?
        .ok_or_else(|| AppError::Asr("笔记不存在，可能已被删除".into()))?;

    // 幂等分支放在拿占位之前：已有转录时连占位都不该占用，重复点瞬间返回。
    if note
        .transcript
        .as_deref()
        .is_some_and(|text| !text.trim().is_empty())
    {
        return Ok(note);
    }

    let Some(file_name) = note.audio_file.clone() else {
        return Err(AppError::Asr("这条笔记还没有录音，无法转写".into()));
    };
    let path = paths::safe_file_in_dir(&paths::notes_audio_dir(), &file_name)
        .ok_or_else(|| AppError::Asr("录音文件名非法".into()))?;
    if !path.is_file() {
        return Err(AppError::Asr(
            "录音文件已不存在，可能已被清理，请重新录音".into(),
        ));
    }

    let _guard = state.note_transcription.try_begin(note_id).ok_or_else(|| {
        // 只影响文案：占位已经被别人拿走，读到的 id 可能刚释放，措辞仍然正确。
        match state.note_transcription.active_note() {
            Some(active) if active == note_id => AppError::Asr("这篇笔记正在转写中，请稍候".into()),
            _ => AppError::Asr("已有另一篇笔记正在转写，请等它完成再试".into()),
        }
    })?;

    let started = Instant::now();
    let engine = paths::read_engine_config();
    let asr_started = Instant::now();
    let result = transcription_service::transcribe_file(
        &app,
        state.inner(),
        &path,
        note.duration_sec,
        "笔记转写失败",
    )
    .await
    .map_err(AppError::Asr)?;
    let asr_ms = Some(elapsed_ms(asr_started));

    let text = result.text.trim().to_string();
    if text.is_empty() {
        return Err(AppError::Asr(
            "这段录音没有识别出文字，请确认录音里有声音后重试".into(),
        ));
    }

    let total_ms = Some(elapsed_ms(started));
    let retention_days = state.with_profile(|profile| profile.history_settings.retention_days);
    let draft = history_draft_for_note(
        now_millis_u64(),
        &text,
        note.duration_sec,
        result.language,
        engine,
        asr_ms,
        total_ms,
    );
    let history_id = history_service::insert(draft, retention_days)
        .await
        .map_err(|error| AppError::Asr(format!("保存转写历史失败: {error}")))?;

    let updated = notes_service::set_transcript(note_id, &text, note.duration_sec, Some(history_id))
        .await
        .map_err(AppError::Asr)?
        .ok_or_else(|| AppError::Asr("转写完成，但这篇笔记已被删除".into()))?;

    let _ = app.emit("history-updated", serde_json::json!({ "id": history_id }));
    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_history_keeps_the_audio_out_of_the_history_table() {
        let draft = history_draft_for_note(
            42,
            "  会议正文  ",
            Some(600.0),
            Some("zh".into()),
            "qwen3-asr-0.6b".into(),
            Some(1_200),
            Some(1_400),
        );

        // audio_file 留空是「不复制音频」的落点：历史一旦持有文件名，删历史
        // 就会连带删掉笔记唯一的录音。
        assert!(draft.audio_file.is_none());
        assert_eq!(draft.mode, "note");
        assert_eq!(draft.workflow, "note");
        assert_eq!(draft.status, "success");
        assert_eq!(draft.duration_sec, Some(600.0));
        assert_eq!(draft.asr_ms, Some(1_200));
        assert_eq!(draft.total_ms, Some(1_400));
    }

    #[test]
    fn note_history_session_id_is_timestamp_like() {
        // 笔记没有录音会话 ID 可用，用毫秒时间戳避免与听写会话（1、2、3…）撞号。
        let id = now_millis_u64();
        assert!(id > 1_000_000, "note session id should be timestamp-like: {id}");
    }
}
