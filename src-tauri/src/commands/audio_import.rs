//! 导入音频转写命令。
//!
//! 与「边录边转」解耦：选择本地音频文件 → 解码为 16k WAV → 交给统一的转录入口
//! → 存入历史，不自动粘贴。
//!
//! 转写细节（引擎分派、原生流式、长音频切片）都在
//! `services::transcription_service`；本命令只负责解码、落库与错误留痕。

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tauri::Emitter;

use crate::services::transcription_events::Preview;
use crate::services::transcription_service::{self, AudioInput, TranscribeOptions};
use crate::services::{audio_import_service, history_service};
use crate::state::AppState;
use crate::utils::{paths, AppError};

fn emit_history_updated(app_handle: &tauri::AppHandle, id: Option<i64>) {
    let _ = app_handle.emit("history-updated", serde_json::json!({ "id": id }));
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

/// 本地引擎实测约 4.6 倍实时（317 秒音频 68.6 秒转完）。取 4 倍并向上取整，
/// 让预估偏保守——报少了用户会以为卡死。
const LOCAL_ESTIMATE_SECS_PER_AUDIO_SEC: f64 = 0.25;

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
    /// 预估转写耗时（秒）。在线引擎依赖网络，不给预估。
    pub estimated_secs: Option<f64>,
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

    let estimated_secs = if transcription_service::is_local_engine(&engine) {
        meta.duration_sec
            .map(|secs| (secs * LOCAL_ESTIMATE_SECS_PER_AUDIO_SEC).ceil())
    } else {
        None
    };

    Ok(AudioProbe {
        duration_sec: meta.duration_sec,
        size_bytes: meta.size_bytes,
        sample_rate: meta.sample_rate,
        engine,
        limit_hint,
        estimated_secs,
    })
}

/// 导入并转写音频，返回历史记录。
///
/// 失败时也会写入一条 `asr_error` 历史记录并正常返回（不抛 Err），
/// 遵循 finalize 的错误留痕惯例；前端按 `status` 判断成败。
#[tauri::command]
pub async fn transcribe_imported_audio(
    app_handle: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<history_service::HistoryRecord, AppError> {
    let started = Instant::now();
    let engine = paths::read_engine_config();
    let session_id = next_import_session_id();
    let retention_days = state.with_profile(|profile| profile.history_settings.retention_days);

    // 解码与引擎上限属于「入参问题」，失败直接抛 Err 让前端提示，不在历史里留痕。
    // 这一次解码的产物同时用于转写与落盘，转写走内存字节，成功后原样存为历史音频。
    let imported = audio_import_service::import_audio_to_wav(std::path::Path::new(&path))?;

    let asr_started = Instant::now();
    let asr_result = transcription_service::transcribe(
        &app_handle,
        state.inner(),
        AudioInput::WavBytes(&imported.wav),
        TranscribeOptions::new("导入转写失败")
            .with_duration(Some(imported.duration_sec))
            .with_preview(Preview::ImportInterim),
    )
    .await;
    let asr_ms = Some(asr_started.elapsed().as_millis().min(u64::MAX as u128) as u64);

    let (status, text, original_text, language, error) = match asr_result {
        Ok(result) => {
            let text = result.text.trim().to_string();
            ("success", text.clone(), text, result.language, None)
        }
        Err(error) => (
            "asr_error",
            String::new(),
            String::new(),
            None,
            Some(error.message()),
        ),
    };

    // 成功才保存音频；失败记录也留痕但不占音频文件。
    let audio_file = if status == "success" {
        match history_service::save_audio(session_id, imported.wav).await {
            Ok(file) => Some(file),
            Err(e) => {
                log::warn!("导入音频保存失败（转写结果仍保留）: {e}");
                None
            }
        }
    } else {
        None
    };

    let total_ms = Some(started.elapsed().as_millis().min(u64::MAX as u128) as u64);
    let new_id = history_service::insert(
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
        retention_days,
    )
    .await
    .map_err(|e| AppError::Asr(format!("保存导入历史失败: {e}")))?;

    emit_history_updated(&app_handle, Some(new_id));

    history_service::get(new_id)
        .await
        .map_err(|e| AppError::Asr(format!("读取导入历史失败: {e}")))?
        .map(|stored| stored.record)
        .ok_or_else(|| AppError::Asr("读取导入历史失败：记录不存在".into()))
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

    // 原生流式会话 id 现在由 transcription_service 统一申请（与录音共用同一个
    // 递增计数器），导入不再自己生成，原先「导入把计数器抬到万亿级」的回归
    // 已无从复现，故相关用例随分支一起删除。
}
