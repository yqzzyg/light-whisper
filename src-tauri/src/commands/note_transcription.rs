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

use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tauri::Emitter;

use crate::services::audio_chunk_service::{self, WavChunker, CHUNK_SECS};
use crate::services::funasr_service::{self, TranscriptionResult};
use crate::services::{history_service, notes_service, transcription_service};
use crate::state::AppState;
use crate::utils::{paths, AppError};

/// 切片转写的进度事件，前端只用它显示「正在转写第 N 块」。
const NOTE_TRANSCRIPTION_PROGRESS_EVENT: &str = "note-transcription-progress";

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
        let result = self.run_asr(&path, recording, error_prefix).await?;
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
        let history_id = match reuse_history {
            Some(existing) => {
                history_service::update_transcription_result(
                    existing,
                    &text,
                    &draft.engine,
                    draft.language,
                    asr_ms,
                    draft.total_ms,
                    draft.duration_sec,
                )
                .await
                .map_err(|error| AppError::Asr(format!("更新转写历史失败: {error}")))?;
                existing
            }
            None => history_service::insert(draft, self.retention_days)
                .await
                .map_err(|error| AppError::Asr(format!("保存转写历史失败: {error}")))?,
        };

        notes_service::set_note_recording_transcript(recording.id, &text, Some(history_id))
            .await
            .map_err(AppError::Asr)?
            .ok_or_else(|| AppError::Asr(format!("{error_prefix}：这篇笔记已被删除")))?;
        let _ = self
            .app
            .emit("history-updated", serde_json::json!({ "id": history_id }));
        Ok(())
    }

    /// 一次 ASR 调用：超长录音走切片，其余沿用整段转写。
    ///
    /// 切片只在本地引擎下生效——在线引擎有硬性的时长/大小上限，`transcribe_file`
    /// 会先给出明确拦截文案，拿不到可供切分的时长预算。
    async fn run_asr(
        &self,
        path: &Path,
        recording: &notes_service::NoteRecording,
        error_prefix: &str,
    ) -> Result<TranscriptionResult, AppError> {
        if audio_chunk_service::should_chunk(recording.duration_sec)
            && transcription_service::is_local_engine(&self.engine)
        {
            return self
                .transcribe_in_chunks(path, recording, error_prefix)
                .await;
        }

        transcription_service::transcribe_file(
            self.app,
            self.state,
            path,
            recording.duration_sec,
            error_prefix,
        )
        .await
        .map_err(AppError::Asr)
    }

    /// 把一段长录音按 30 分钟切开，逐块转写后拼成一段文本。
    ///
    /// 切的是「这一段的音频数据」，不新增 `note_recordings`、不多写历史：
    /// 用户看到的仍是一段录音、一段转录。中间任何一块失败都返回错误，整段
    /// 结果不落库（重试从头开始）——半截转录比没有更糟，用户会以为录丢了。
    async fn transcribe_in_chunks(
        &self,
        path: &Path,
        recording: &notes_service::NoteRecording,
        error_prefix: &str,
    ) -> Result<TranscriptionResult, AppError> {
        let mut chunker = WavChunker::open(path, CHUNK_SECS)
            .map_err(|error| AppError::Asr(format!("{error_prefix}：{error}")))?;
        let sample_rate = chunker.sample_rate();
        // 只用于进度文案；静音吸附会让实际块数略有出入，别拿它当循环上界。
        let estimated = audio_chunk_service::chunk_count(recording.duration_sec.unwrap_or(0.0));

        let mut parts: Vec<String> = Vec::new();
        let mut language: Option<String> = None;
        let mut index = 0usize;

        while let Some(samples) = chunker
            .next_chunk()
            .map_err(|error| AppError::Asr(format!("{error_prefix}：{error}")))?
        {
            index += 1;
            let _ = self.app.emit(
                NOTE_TRANSCRIPTION_PROGRESS_EVENT,
                serde_json::json!({
                    "noteId": recording.note_id,
                    "recordingId": recording.id,
                    "chunk": index,
                    "chunkTotal": estimated,
                }),
            );

            let result =
                funasr_service::transcribe_pcm16(self.state, &samples, sample_rate, self.app)
                    .await
                    .map_err(|error| {
                        AppError::Asr(format!("{error_prefix}：第 {index} 块转写失败：{error}"))
                    })?;
            if !result.success {
                let reason = result.error.unwrap_or_else(|| "未知错误".into());
                return Err(AppError::Asr(format!(
                    "{error_prefix}：第 {index} 块转写失败：{reason}"
                )));
            }
            if language.is_none() {
                language = result.language.filter(|value| !value.trim().is_empty());
            }
            parts.push(result.text);
        }

        if parts.is_empty() {
            return Err(AppError::Asr(format!(
                "{error_prefix}：录音文件里没有可用的音频数据"
            )));
        }

        Ok(TranscriptionResult {
            text: audio_chunk_service::join_transcripts(&parts),
            duration: recording.duration_sec,
            success: true,
            error: None,
            language,
        })
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
