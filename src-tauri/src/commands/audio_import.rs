//! 导入音频转写命令。
//!
//! 与「边录边转」解耦：选择本地音频文件 → 解码为 16k WAV → 落盘 → 交给统一的转录
//! 入口 → 存入历史，不自动粘贴。落盘排在转写之前，是为了让转录层走路径直传与切片，
//! Rust 侧不必再留一份整段 PCM。
//!
//! 转写细节（引擎分派、长音频切片）都在 `services::transcription_service`；
//! 本命令只负责解码、落盘与错误留痕。

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tauri::Emitter;

use crate::services::history_service::SpeakerSegment;
use crate::services::transcription_service::{self, AudioInput, TranscribeOptions};
use crate::services::{audio_import_service, history_service};
use crate::state::AppState;
use crate::utils::{paths, AppError};

fn emit_history_updated(app_handle: &tauri::AppHandle, id: Option<i64>) {
    let _ = app_handle.emit("history-updated", serde_json::json!({ "id": id }));
}

/// 一段带说话人的转写，camelCase 直供前端渲染。
///
/// 与 `history_service::SpeakerSegment` 分开是有意的：那个结构是 Python IPC 的
/// 反序列化目标（snake_case），这个结构是给 WebView 的展示契约。混用一个结构
/// 迟早要在 serde 属性上二选一，然后 IPC 解析就会悄悄失败。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportedSpeakerSegment {
    pub speaker_id: i64,
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
    pub overlap: bool,
}

impl From<SpeakerSegment> for ImportedSpeakerSegment {
    fn from(segment: SpeakerSegment) -> Self {
        Self {
            speaker_id: segment.speaker_id,
            start_ms: segment.start_ms,
            end_ms: segment.end_ms,
            text: segment.text,
            overlap: segment.overlap,
        }
    }
}

/// 导入转写的结果：历史记录 + 说话人分段 + 降级原因。
///
/// 不把分段塞进 `HistoryRecord`：那个结构被历史列表、剪贴板、笔记等一堆地方共用，
/// 加一个只在导入页有用的字段会让每次列表查询都白读一遍分段表。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportedTranscription {
    pub record: history_service::HistoryRecord,
    /// 按时间排好序的分段；空表示这次没有分离。
    pub segments: Vec<ImportedSpeakerSegment>,
    /// 没做分离的原因码（`disabled` / `online_engine` / `too_long` /
    /// `models_missing` …）；`None` 表示真的分离了。
    pub fallback_reason: Option<String>,
}

/// 分配一个导入专用的 session_id。
///
/// 用毫秒时间戳（远大于录音会话从 1 递增的小整数），避免与录音会话 ID 碰撞，
/// 同时不触碰录音状态机的 session_counter（那会干扰 stale 检测）。
fn next_import_session_id() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(u64::MAX)
}

/// 选择本地音频文件，返回路径（复用 funasr.rs::pick_folder 写法）。
#[tauri::command]
pub async fn pick_audio_file(window: tauri::WebviewWindow) -> Result<Option<String>, AppError> {
    let result = rfd::AsyncFileDialog::new()
        .set_parent(&window)
        .add_filter("音频文件", audio_import_service::SUPPORTED_EXTENSIONS)
        .pick_file()
        .await;
    Ok(result.map(|file| paths::strip_win_prefix(file.path())))
}

/// 「选完文件先回显、用户确认再转写」用的音频信息。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioProbe {
    /// 时长（秒）。容器给不出帧数信息时为 `None`。
    pub duration_sec: Option<f64>,
    /// 源文件字节数。
    pub size_bytes: u64,
    /// 源文件采样率；转写时会统一重采样到 16k。
    pub sample_rate: u32,
    pub engine: String,
    /// 当前引擎转不了这段音频时的拦截文案；`None` 表示可以转。
    pub limit_hint: Option<String>,
}

/// 探测音频文件信息，不转写、不写历史、不占 ASR 锁。
///
/// 只读容器头，所以选完文件能立刻返回；真正的解码与上限拦截在
/// [`transcribe_imported_audio`] 里还会各做一次，这里只是提前告诉用户要不要等。
#[tauri::command]
pub async fn probe_audio_file(path: String) -> Result<AudioProbe, AppError> {
    let engine = paths::read_engine_config();
    let meta = audio_import_service::probe_audio(std::path::Path::new(&path))?;

    // 在线引擎的上限按「解码后的 WAV 大小」判定：16k 单声道 16bit = 32000 字节/秒。
    // 用源文件大小会低估 mp3 转 WAV 后的体积，所以这里按估算值算。
    let wav_len_estimate = meta
        .duration_sec
        .map(|secs| (secs * 32_000.0) as usize)
        .unwrap_or(meta.size_bytes.min(usize::MAX as u64) as usize);
    let limit_hint = transcription_service::check_engine_limit(
        wav_len_estimate,
        meta.duration_sec.unwrap_or(0.0),
    )
    .err()
    .map(|error| error.message());

    Ok(AudioProbe {
        duration_sec: meta.duration_sec,
        size_bytes: meta.size_bytes,
        sample_rate: meta.sample_rate,
        engine,
        limit_hint,
    })
}

/// 导入并转写音频，返回历史记录、说话人分段与降级原因。
///
/// 分离在这里是**自动**的：本地引擎、开关开着、时长不超过两小时就分，否则
/// 退回整段转写并在 `fallback_reason` 里带回原因（前端据此提示，不弹错）。
///
/// 失败时也会写入一条 `asr_error` 历史记录并正常返回（不抛 Err），
/// 遵循 finalize 的错误留痕惯例；前端按 `record.status` 判断成败。
#[tauri::command]
pub async fn transcribe_imported_audio(
    app_handle: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<ImportedTranscription, AppError> {
    let started = Instant::now();
    let engine = paths::read_engine_config();
    let session_id = next_import_session_id();
    let retention_days = state.with_profile(|profile| profile.history_settings.retention_days);

    // 解码与引擎上限属于「入参问题」，失败直接抛 Err 让前端提示，不在历史里留痕。
    let imported = audio_import_service::import_audio_to_wav(std::path::Path::new(&path))?;

    // 先落盘再转写。音频最终本来就要存成历史文件，提前写一次让转录层能走路径直传：
    // Rust 侧因此不必再留一份整段 WAV，长音频也能进切片，内存不再随录音长度翻倍。
    // 代价是落盘成了转写的前置条件——磁盘不可写时转写也没有意义（存不下结果），
    // 所以这里直接当入参问题报错，而不是像以前那样静默丢掉音频。
    let audio_file = history_service::save_audio(session_id, imported.wav)
        .await
        .map_err(|error| AppError::Asr(format!("导入转写失败：保存音频失败: {error}")))?;
    let audio_path = history_service::audio_path(&audio_file)
        .ok_or_else(|| AppError::Asr("导入转写失败：历史音频路径无效".into()))?;

    let asr_started = Instant::now();
    // 走「可能分离」的统一入口：它内部先判条件再拿锁，条件不满足就转发给
    // 普通转写并把原因带回来。命令层不判断引擎、不判断开关、不判断时长。
    let transcription = transcription_service::transcribe_with_speakers(
        &app_handle,
        state.inner(),
        AudioInput::File(&audio_path),
        TranscribeOptions::new("导入转写失败").with_duration(Some(imported.duration_sec)),
    )
    .await;
    let asr_ms = Some(asr_started.elapsed().as_millis().min(u64::MAX as u128) as u64);

    let (status, text, original_text, language, error, segments, fallback_reason) =
        match transcription {
            Ok(outcome) => {
                // 分离成功时这里已经是「说话人 1：…\n说话人 2：…」的多行正文，
                // 所以历史里存的正文本身就带说话人，历史页不用另做渲染。
                let text = outcome.result.text.trim().to_string();
                (
                    "success",
                    text.clone(),
                    text,
                    outcome.result.language,
                    None,
                    outcome.segments,
                    outcome
                        .fallback_reason
                        .map(|reason| reason.code().to_string()),
                )
            }
            Err(error) => (
                "asr_error",
                String::new(),
                String::new(),
                None,
                Some(error.message()),
                Vec::new(),
                None,
            ),
        };

    if let Some(reason) = fallback_reason.as_deref() {
        log::info!("导入转写未做说话人分离: {reason}");
    }

    // 成功则沿用已落盘的文件；失败把音频删掉，失败记录只留痕、不占文件。
    let audio_file = if status == "success" {
        Some(audio_file)
    } else {
        history_service::discard_audio(&audio_file).await;
        None
    };

    let total_ms = Some(started.elapsed().as_millis().min(u64::MAX as u128) as u64);
    let response_segments: Vec<ImportedSpeakerSegment> =
        segments.iter().cloned().map(Into::into).collect();
    let new_id = history_service::insert_with_segments(
        history_service::HistoryDraft {
            session_id,
            mode: "import".into(),
            workflow: "import".into(),
            status: status.into(),
            text,
            original_text,
            source_text: None,
            duration_sec: Some(imported.duration_sec),
            language,
            engine,
            provider: None,
            model: None,
            app_process: None,
            app_window_title: None,
            app_rule_name: None,
            audio_file,
            asr_ms,
            polish_ms: None,
            total_ms,
            error,
            reprocessed_from_id: None,
        },
        segments,
        retention_days,
    )
    .await
    .map_err(|e| AppError::Asr(format!("保存导入历史失败: {e}")))?;

    emit_history_updated(&app_handle, Some(new_id));

    let record = history_service::get(new_id)
        .await
        .map_err(|e| AppError::Asr(format!("读取导入历史失败: {e}")))?
        .map(|stored| stored.record)
        .ok_or_else(|| AppError::Asr("读取导入历史失败：记录不存在".into()))?;

    Ok(ImportedTranscription {
        record,
        segments: response_segments,
        fallback_reason,
    })
}

/// 读一条历史的说话人分段。
///
/// 上传页刷新、或从历史里重新打开一条记录时用它取分段。返回空表就是没分离过，
/// 与笔记侧的 `list_note_speaker_segments` 同一套语义。
#[tauri::command]
pub async fn list_history_speaker_segments(
    history_id: i64,
) -> Result<Vec<ImportedSpeakerSegment>, AppError> {
    let segments = history_service::list_segments(history_id)
        .await
        .map_err(AppError::Asr)?;
    Ok(segments.into_iter().map(Into::into).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_session_id_is_far_larger_than_recording_ids() {
        // 录音会话 ID 从 1 递增，导入用毫秒时间戳，应远大于小整数范围。
        let id = next_import_session_id();
        assert!(
            id > 1_000_000,
            "import session id should be timestamp-like, got {id}"
        );
    }
}
