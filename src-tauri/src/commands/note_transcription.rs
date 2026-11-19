//! 笔记转写命令：把已落盘的会议录音整段转成文字，再写回笔记。
//!
//! 与听写链路的边界：
//!
//! - **不复制音频**：直接把 `notes_audio/` 里的文件路径交给引擎，历史行
//!   `audio_file` 留空（音频归笔记，不归历史）。
//! - **段级幂等**：一条笔记可以有多段录音；每段 `transcript` 非空就跳过，
//!   重复点不会重跑 ASR、也不会多一条历史。
//! - **一次一条**：`AppState::note_transcription` 挡住连点造成的并发转写。
//! - **失败不落历史**：转写失败的段不写历史；之前成功的段保留，重试只补没转的。

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

/// 一次转写运行里不变的上下文：引擎、保留策略、起始时刻。
///
/// 打包成结构体而不是一路传参，是因为整篇转写和单段重转都要带同一批值；
/// 分开传会让内部函数长到 8 个参数（clippy `too_many_arguments`）。
struct TranscribeRun<'a> {
    app: &'a tauri::AppHandle,
    state: &'a AppState,
    engine: String,
    retention_days: u32,
    /// 整批任务的开始时刻，每段历史的 `total_ms` 从这里算起。
    started: Instant,
}

impl TranscribeRun<'_> {
    /// 转写一段录音：校验路径 → 调引擎 → 写一条历史 → 把转录写回该段。
    ///
    /// 整篇转写与单段重转共用这一段逻辑。
    async fn transcribe(
        &self,
        recording: &notes_service::NoteRecording,
        error_prefix: &str,
        session_id: u64,
    ) -> Result<(), AppError> {
        let path = paths::safe_file_in_dir(&paths::notes_audio_dir(), &recording.audio_file)
            .ok_or_else(|| AppError::Asr(format!("{error_prefix}：录音文件名非法")))?;
        if !path.is_file() {
            return Err(AppError::Asr(format!(
                "{error_prefix}：录音文件已不存在，可能已被清理，请重新录音"
            )));
        }

        let asr_started = Instant::now();
        let result = transcription_service::transcribe_file(
            self.app,
            self.state,
            &path,
            recording.duration_sec,
            error_prefix,
        )
        .await
        .map_err(AppError::Asr)?;
        let asr_ms = Some(elapsed_ms(asr_started));

        let text = result.text.trim().to_string();
        if text.is_empty() {
            return Err(AppError::Asr(format!(
                "{error_prefix}：这段录音没有识别出文字，请确认录音里有声音后重试"
            )));
        }

        let draft = history_draft_for_note(
            session_id,
            &text,
            recording.duration_sec,
            result.language,
            self.engine.clone(),
            asr_ms,
            Some(elapsed_ms(self.started)),
        );
        let history_id = history_service::insert(draft, self.retention_days)
            .await
            .map_err(|error| AppError::Asr(format!("保存转写历史失败: {error}")))?;

        notes_service::set_note_recording_transcript(recording.id, &text, Some(history_id))
            .await
            .map_err(AppError::Asr)?
            .ok_or_else(|| AppError::Asr(format!("{error_prefix}：这篇笔记已被删除")))?;
        let _ = self
            .app
            .emit("history-updated", serde_json::json!({ "id": history_id }));
        Ok(())
    }
}

/// 把一篇笔记里所有还没转写的录音段转成文本。
///
/// 段与段顺序执行，每段写一条 `mode="note"` 历史与段转录；全部完成后笔记的
/// `transcript` 是各段拼接的结果。转写期间可以切走页面：命令本身不会被取消，
/// 前端回到笔记时重读即可看到结果。
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

    let recordings = notes_service::list_note_recordings(note_id)
        .await
        .map_err(AppError::Asr)?;
    // 带上段在笔记里的真实序号：待转写的不一定是第 1 段，报错时要说对是哪一段。
    let pending: Vec<(usize, &notes_service::NoteRecording)> = recordings
        .iter()
        .enumerate()
        .filter(|(_, recording)| {
            recording
                .transcript
                .as_deref()
                .is_none_or(|text| text.trim().is_empty())
        })
        .collect();
    // 没有待转写的段就直接返回：包括 P2 从历史转存来的笔记（音频不在笔记里）。
    if pending.is_empty() {
        return Ok(note);
    }

    let _guard = state.note_transcription.try_begin(note_id).ok_or_else(|| {
        // 只影响文案：占位已经被别人拿走，读到的 id 可能刚释放，措辞仍然正确。
        match state.note_transcription.active_note() {
            Some(active) if active == note_id => AppError::Asr("这篇笔记正在转写中，请稍候".into()),
            _ => AppError::Asr("已有另一篇笔记正在转写，请等它完成再试".into()),
        }
    })?;

    let run = TranscribeRun {
        app: &app,
        state: state.inner(),
        engine: paths::read_engine_config(),
        retention_days: state.with_profile(|profile| profile.history_settings.retention_days),
        started: Instant::now(),
    };

    for (index, (position, recording)) in pending.iter().enumerate() {
        let label = format!("笔记第 {} 段转写失败", position + 1);
        let session_id = now_millis_u64().saturating_add(index as u64);
        run.transcribe(recording, &label, session_id).await?;
    }

    notes_service::get_note(note_id)
        .await
        .map_err(AppError::Asr)?
        .ok_or_else(|| AppError::Asr("转写完成，但这篇笔记已被删除".into()))
}

/// 只重转指定的一段录音。
///
/// 与整篇转录的区别：这里**不看幂等**——段的 `transcript` 已存在也要重跑，供
/// 用户挑出识别不佳的单段重来。并发仍按 note_id 占位，避免与整篇转录互相踩。
/// 重转会新增一条历史记录（历史是追加日志），不删旧的那条。
#[tauri::command]
pub async fn transcribe_note_recording_segment(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    recording_id: i64,
) -> Result<notes_service::NoteDetail, AppError> {
    let recording = notes_service::get_note_recording(recording_id)
        .await
        .map_err(AppError::Asr)?
        .ok_or_else(|| AppError::Asr("录音片段不存在或已被删除".into()))?;
    let note_id = recording.note_id;

    let _guard = state.note_transcription.try_begin(note_id).ok_or_else(|| {
        match state.note_transcription.active_note() {
            Some(active) if active == note_id => AppError::Asr("这篇笔记正在转写中，请稍候".into()),
            _ => AppError::Asr("已有另一篇笔记正在转写，请等它完成再试".into()),
        }
    })?;

    let run = TranscribeRun {
        app: &app,
        state: state.inner(),
        engine: paths::read_engine_config(),
        retention_days: state.with_profile(|profile| profile.history_settings.retention_days),
        started: Instant::now(),
    };
    run.transcribe(&recording, "这段录音转写失败", now_millis_u64())
        .await?;

    notes_service::get_note(note_id)
        .await
        .map_err(AppError::Asr)?
        .ok_or_else(|| AppError::Asr("转写完成，但这篇笔记已被删除".into()))
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
        assert!(
            id > 1_000_000,
            "note session id should be timestamp-like: {id}"
        );
    }
}
