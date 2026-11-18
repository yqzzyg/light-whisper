//! 导入音频转写命令。
//!
//! 与「边录边转」解耦：选择本地音频文件 → 解码为 16k WAV → 按当前引擎转写
//! → 存入历史，不自动粘贴。
//!
//! 本地引擎（qwen3 / r2t2）走原生流式：按块喂入PCM，边转边通过
//! `transcription-result` 事件推送 interim（负数 sessionId，浮层与主界面都会忽略，
//! 只有上传页 hook 消费）；在线引擎（GLM / DashScope）不支持流式，转完一次性出结果。

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tauri::Emitter;
use tokio::sync::MutexGuard;

use crate::services::audio_service::next_native_session_id;
use crate::services::funasr_service::native_stream::NativeStreamClient;
use crate::services::{
    audio_import_service, funasr_service, history_service, transcription_service,
};
use crate::state::AppState;
use crate::utils::{paths, AppError};

/// 流式喂入块大小：16000 样本 = 1 秒。
const STREAM_FEED_SAMPLES: usize = 16_000;

/// 导入流式结果的 sessionId 上界取负数，确保浮层（sessionId >= 0 才处理）与
/// 主界面（interim 直接丢弃）都不会把导入结果当成录音字幕。
const IMPORT_SESSION_ID_BASE: i64 = -1_000_000;

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

/// 流式中间结果用的负数 sessionId：每次导入递减，避免浮层/主界面误判为录音会话。
fn next_import_stream_session_id() -> i64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    IMPORT_SESSION_ID_BASE.saturating_sub(millis)
}

/// 当前引擎是否走本地原生流式（支持边转边出 interim）。
fn uses_native_streaming(engine: &str) -> bool {
    matches!(engine, "qwen3-asr-0.6b" | "confucius4-r2t2")
}

/// 把一次流式更新推给上传页。用负数 sessionId + interim=true，
/// 保证只有导入页消费，浮层与录音主界面都不会显示。
fn emit_import_interim(
    app_handle: &tauri::AppHandle,
    stream_session_id: i64,
    update: &crate::services::funasr_service::native_stream::NativeStreamUpdate,
) {
    let text = format!("{}{}", update.text, update.tentative_text);
    let _ = app_handle.emit(
        "transcription-result",
        serde_json::json!({
            "sessionId": stream_session_id,
            "text": text,
            "stableText": update.text,
            "tentativeText": update.tentative_text,
            "interim": true,
            "language": update.language,
        }),
    );
}

/// 本地引擎流式转写：持有 ASR 所有权锁，按块喂入 PCM 并推送 interim，最后 finish。
///
/// 返回 `Result<TranscriptionResult, String>`，与 `transcribe_wav_bytes_dispatched`
/// 保持一致，方便上层统一处理成功/失败分支。
async fn transcribe_import_streaming(
    app_handle: &tauri::AppHandle,
    asr_guard: MutexGuard<'_, ()>,
    state: &AppState,
    samples: &[i16],
    stream_session_id: i64,
) -> Result<funasr_service::TranscriptionResult, String> {
    // 持有锁直到 finish，期间全局热键触发的录音会排队等待，避免抢占同一 ASR 会话。
    let _guard = asr_guard;

    if !state.is_funasr_ready() {
        return Err("语音识别引擎尚未就绪，请等待初始化完成".into());
    }

    // 原生会话 id 必须与录音共用同一个单调递增计数器：常驻 Python 引擎会拒绝
    // 不大于上一次的 session id。这里不能用时间戳，否则会把计数上限抬到万亿级，
    // 之后所有录音（1、2、3…）都会被当成旧会话拒绝。
    let native_session_id = next_native_session_id();
    let mut client = NativeStreamClient::start(
        state,
        Some(app_handle),
        native_session_id,
        String::new(),
        None,
    )
    .await
    .map_err(|error| format!("导入转写失败: {error}"))?;

    for chunk in samples.chunks(STREAM_FEED_SAMPLES) {
        let update = match client.feed(state, Some(app_handle), chunk).await {
            Ok(update) => update,
            Err(error) => {
                // 喂入失败要释放引擎侧会话，避免占用。
                let _ = client.cancel(state, Some(app_handle)).await;
                return Err(format!("导入转写失败: {error}"));
            }
        };
        emit_import_interim(app_handle, stream_session_id, &update);
    }

    client
        .finish(state, Some(app_handle))
        .await
        .map_err(|error| format!("导入转写失败: {error}"))
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

/// 校验解码后的 WAV 是否超出当前引擎的上限。
///
/// 判据在 `transcription_service` 里（导入与笔记转写共用同一套文案）。
fn check_engine_limit(wav: &[u8], duration_sec: f64) -> Result<(), AppError> {
    transcription_service::check_engine_limit(wav.len(), duration_sec)
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
    let file_path = std::path::Path::new(&path);
    let engine = paths::read_engine_config();
    let session_id = next_import_session_id();
    let retention_days = state.with_profile(|profile| profile.history_settings.retention_days);

    // 解码与引擎上限属于「入参问题」，直接抛 Err 让前端提示，不留痕。
    let imported = audio_import_service::import_audio_to_wav(file_path)?;
    check_engine_limit(&imported.wav, imported.duration_sec)?;

    let asr_started = Instant::now();
    let asr_result = if uses_native_streaming(&engine) {
        // 本地引擎：解码一次得到 16k PCM，按块喂入原生流式，边转边推 interim。
        // 单独解码而非复用 WAV 字节，是为了直接拿到 i16 样本，避免再解一次 WAV 头。
        match audio_import_service::decode_to_16k_mono(file_path) {
            Ok(decoded) => {
                let asr_guard = state.engine.native_asr_owner.lock().await;
                transcribe_import_streaming(
                    &app_handle,
                    asr_guard,
                    state.inner(),
                    &decoded.samples,
                    next_import_stream_session_id(),
                )
                .await
            }
            Err(error) => Err(error.to_string()),
        }
    } else {
        // 在线引擎不支持流式：维持一次性转写，转完才在上传页出结果。
        transcription_service::transcribe_wav_bytes(
            &app_handle,
            state.inner(),
            imported.wav.clone(),
            "导入转写失败",
        )
        .await
    };
    let asr_ms = Some(asr_started.elapsed().as_millis().min(u64::MAX as u128) as u64);

    let (status, text, original_text, language, error) = match asr_result {
        Ok(result) => (
            "success",
            result.text.trim().to_string(),
            result.text.trim().to_string(),
            result.language,
            None,
        ),
        Err(message) => (
            "asr_error",
            String::new(),
            String::new(),
            None,
            Some(message),
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

    #[test]
    fn import_stream_session_id_is_negative_and_distinct_from_history_id() {
        // 流式 sessionId 必须为负数，浮层（>=0 才处理）与主界面（丢弃 interim）
        // 都不应把导入结果当成录音字幕。
        let stream_id = next_import_stream_session_id();
        assert!(
            stream_id < 0,
            "stream session id must be negative: {stream_id}"
        );
        assert!(
            stream_id.unsigned_abs() > next_import_session_id() / 2,
            "stream id magnitude should be timestamp-like"
        );
    }

    #[test]
    fn import_and_recording_share_monotonic_native_session_ids() {
        // 回归：导入曾用毫秒时间戳当原生会话 id，导致常驻引擎会话上限被抬到
        // 万亿级，之后所有录音（1、2、3…）都被拒绝为旧会话。导入必须与录音
        // 共用同一个递增计数器。
        let import_id = next_native_session_id();
        let recording_id = next_native_session_id();
        assert!(
            recording_id > import_id,
            "native session ids must keep increasing across import and recording"
        );
    }

    #[test]
    fn only_local_engines_use_native_streaming() {
        assert!(uses_native_streaming("qwen3-asr-0.6b"));
        assert!(uses_native_streaming("confucius4-r2t2"));
        assert!(!uses_native_streaming("glm-asr"));
        assert!(!uses_native_streaming("alibaba-asr"));
    }
}
