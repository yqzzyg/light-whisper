//! 笔记转写命令：把已落盘的会议录音整段转成文字，再写回笔记。
//!
//! 与听写链路的边界：
//!
//! - **不复制音频**：直接把 `notes_audio/` 里的文件路径交给引擎，历史行
//!   `audio_file` 留空（音频归笔记，不归历史）。
//! - **段级幂等**：一条笔记可以有多段录音；每段 `transcript` 非空就跳过，
//!   重复点不会重跑 ASR、也不会多一条历史。显式 `force`（重新转写）才覆盖。
//! - **一次一条**：`AppState::note_transcription` 挡住连点造成的并发转写。
//! - **失败不落历史**：转写失败的段不写历史；之前成功的段保留，重试只补没转的。
//! - **说话人分离是旁路**：走 `transcribe_with_speakers`，分离不可用时它内部
//!   降级成整段转写，这里拿到的就是「零分段」——本命令不做 `match`。

use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tauri::Emitter;

use crate::services::transcription_events::Preview;
use crate::services::transcription_service::{
    self, AudioInput, SpeakerTranscription, TranscribeOptions,
};
use crate::services::{history_service, notes_service};
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
    /// 整篇转写与单段重转共用这一段逻辑。`reuse_history` 有值时**就地更新**那条
    /// 历史，而不是新插一行：重转同一段录音不该在历史里堆出多条重复记录，否则
    /// 段表只能指向最新一条，旧记录点「打开笔记」会变成「笔记已删除」。
    async fn transcribe(
        &self,
        recording: &notes_service::NoteRecording,
        error_prefix: &str,
        session_id: u64,
        reuse_history: Option<i64>,
    ) -> Result<(), AppError> {
        let path = paths::safe_file_in_dir(&paths::notes_audio_dir(), &recording.audio_file)
            .ok_or_else(|| AppError::Asr(format!("{error_prefix}：录音文件名非法")))?;
        if !path.is_file() {
            return Err(AppError::Asr(format!(
                "{error_prefix}：录音文件已不存在，可能已被清理，请重新录音"
            )));
        }

        let asr_started = Instant::now();
        let outcome = self.run_asr(&path, recording, error_prefix).await?;
        let asr_ms = Some(elapsed_ms(asr_started));

        let text = outcome.result.text.trim().to_string();
        if text.is_empty() {
            return Err(AppError::Asr(format!(
                "{error_prefix}：这段录音没有识别出文字，请确认录音里有声音后重试"
            )));
        }
        if let Some(skip) = outcome.fallback_reason {
            log::info!("笔记第 {} 段未做说话人分离：{}", recording.id, skip.code());
        } else {
            // 分离统计只落日志：它是排查用（几人、语音占比），界面上有分段就够。
            log::info!(
                "笔记第 {} 段分离出 {} 段，统计 {:?}",
                recording.id,
                outcome.segments.len(),
                outcome.diarization
            );
        }

        let draft = history_draft_for_note(
            session_id,
            &text,
            recording.duration_sec,
            outcome.result.language,
            self.engine.clone(),
            asr_ms,
            Some(elapsed_ms(self.started)),
        );
        // 历史行、该段转录、说话人分段在同一个事务里落库：笔记被删时历史一起回滚，
        // 不会留下没人认领的记录；重新转写则就地更新原来的历史行并整段替换分段。
        let history_id = notes_service::save_recording_transcription_with_segments(
            recording.id,
            draft,
            reuse_history,
            self.retention_days,
            outcome.segments,
        )
        .await
        .map_err(|error| AppError::Asr(format!("{error_prefix}：{error}")))?;
        let _ = self
            .app
            .emit("history-updated", serde_json::json!({ "id": history_id }));
        Ok(())
    }

    /// 一次 ASR 调用，带说话人分离。
    ///
    /// 分离的所有取舍都在 `transcription_service` 里：这里只负责把时长与进度目标
    /// 告诉它，拿回「文本 + 分段（可能为空）+ 降级原因（可能为 None）」。
    /// 超过两小时的录音、没装模型、在线引擎都会在那里降级，本层不判断。
    async fn run_asr(
        &self,
        path: &Path,
        recording: &notes_service::NoteRecording,
        error_prefix: &str,
    ) -> Result<SpeakerTranscription, AppError> {
        transcription_service::transcribe_with_speakers(
            self.app,
            self.state,
            AudioInput::File(path),
            TranscribeOptions::new(error_prefix)
                .with_duration(recording.duration_sec)
                .with_preview(Preview::ChunkProgress {
                    note_id: recording.note_id,
                    recording_id: recording.id,
                }),
        )
        .await
    }
}

/// 把一篇笔记里所有还没转写的录音段转成文本。
///
/// 段与段顺序执行，每段写一条 `mode="note"` 历史与段转录；全部完成后笔记的
/// `transcript` 是各段拼接的结果。转写期间可以切走页面：命令本身不会被取消，
/// 前端回到笔记时重读即可看到结果。
///
/// `force = true` 是「重新转写」：不再跳过已有转录的段，全部重跑并**覆盖**原有
/// 转录。已经写过历史的段会就地更新那条历史（不会堆重复记录），换了引擎重转就
/// 靠这个。失效场景：从历史/上传转存来的笔记没有录音段，重转无从下手，给明确提示。
#[tauri::command]
pub async fn transcribe_note_recording(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    note_id: i64,
    force: Option<bool>,
) -> Result<notes_service::NoteDetail, AppError> {
    let force = force.unwrap_or(false);
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
            force
                || recording
                    .transcript
                    .as_deref()
                    .is_none_or(|text| text.trim().is_empty())
        })
        .collect();
    // 没有待转写的段就直接返回：包括 P2 从历史转存来的笔记（音频不在笔记里）。
    if pending.is_empty() {
        if force && recordings.is_empty() {
            return Err(AppError::Asr(
                "这篇笔记的转录来自历史记录，没有可重新转写的录音".into(),
            ));
        }
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
        // 重转时沿用该段原来的历史行，避免历史里留下指向旧文本的死链。
        let reuse_history = if force { recording.history_id } else { None };
        run.transcribe(recording, &label, session_id, reuse_history)
            .await?;
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
/// 重转就地更新该段原来的历史行（没有才新插），不在历史里堆重复记录。
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
    run.transcribe(
        &recording,
        "这段录音转写失败",
        now_millis_u64(),
        recording.history_id,
    )
    .await?;

    notes_service::get_note(note_id)
        .await
        .map_err(AppError::Asr)?
        .ok_or_else(|| AppError::Asr("转写完成，但这篇笔记已被删除".into()))
}

/// 一条笔记里的说话人分段，带着「属于哪段录音」。
///
/// 前端要按录音分组渲染（用户能看到每一段录音各自的对话），所以这里不把多段录音
/// 的分段混成一个时间轴——各段录音的时间都是「从 0 开始」的，混起来会重叠。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NoteSpeakerSegment {
    pub recording_id: i64,
    pub speaker_id: i64,
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
    pub overlap: bool,
}

/// 读一条笔记所有录音的说话人分段。
///
/// 按录音创建顺序 + 段内时间排序。返回空表就是「这次没有分离」：可能是功能关了、
/// 引擎是在线的、录音太长、模型没装，或者分离那一步失败降级了——这些都是正常
/// 状态，不是错误，所以这里不返回 `Result::Err` 去逼前端弹错误框。
#[tauri::command]
pub async fn list_note_speaker_segments(note_id: i64) -> Result<Vec<NoteSpeakerSegment>, AppError> {
    let recordings = notes_service::list_note_recordings(note_id)
        .await
        .map_err(AppError::Asr)?;
    let mut all = Vec::new();
    for recording in recordings {
        let segments = notes_service::list_recording_segments(recording.id)
            .await
            .map_err(AppError::Asr)?;
        all.extend(segments.into_iter().map(|segment| NoteSpeakerSegment {
            recording_id: recording.id,
            speaker_id: segment.speaker_id,
            start_ms: segment.start_ms,
            end_ms: segment.end_ms,
            text: segment.text,
            overlap: segment.overlap,
        }));
    }
    Ok(all)
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
