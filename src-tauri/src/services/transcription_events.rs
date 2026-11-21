//! 转写过程的 UI 事件。
//!
//! [`Preview`] 只表达调用方的意图；事件名、payload 形状，以及「导入结果必须用
//! 负数 sessionId，免得被录音浮层当成实时字幕」这类展示约定全部收在这里。
//! 这样 ASR 分派层不必知道前端有几个页面、哪些事件有人听。

use std::time::{SystemTime, UNIX_EPOCH};

use tauri::Emitter;

use super::funasr_service::native_stream::NativeStreamUpdate;

/// 转写过程中要推送的预览。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preview {
    /// 不推送任何进度。
    None,
    /// 上传页：逐段实时文本。
    ImportInterim,
    /// 笔记：切片转写到第几块。
    ChunkProgress { note_id: i64, recording_id: i64 },
}

/// 导入流式结果的 sessionId 上界取负数：浮层只处理 `sessionId >= 0`，主界面直接
/// 丢弃 interim，两者都不会把导入结果当成录音字幕。
const IMPORT_SESSION_ID_BASE: i64 = -1_000_000;

/// 笔记切片进度事件名。
pub const NOTE_PROGRESS_EVENT: &str = "note-transcription-progress";

/// 每次导入一个独立且为负的 sessionId，避免与录音会话（从 1 递增）碰撞。
fn next_import_session_id() -> i64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    IMPORT_SESSION_ID_BASE.saturating_sub(millis)
}

/// 一次导入的实时文本通道。
///
/// sessionId 在开通时确定一次、整轮复用：每块都换 id 会让前端无法判断这些
/// interim 属于同一次转写。
pub struct InterimChannel {
    session_id: i64,
}

impl InterimChannel {
    pub fn open() -> Self {
        Self {
            session_id: next_import_session_id(),
        }
    }

    /// 推送一段实时文本。`tentative_text` 尚未定稿，拼在稳定文本之后展示。
    pub fn emit(&self, app_handle: &tauri::AppHandle, update: &NativeStreamUpdate) {
        let text = format!("{}{}", update.text, update.tentative_text);
        let _ = app_handle.emit(
            "transcription-result",
            serde_json::json!({
                "sessionId": self.session_id,
                "text": text,
                "stableText": update.text,
                "tentativeText": update.tentative_text,
                "interim": true,
                "language": update.language,
            }),
        );
    }
}

/// 推送切片进度；`preview` 不是 [`Preview::ChunkProgress`] 时什么也不做。
pub fn emit_chunk_progress(
    app_handle: &tauri::AppHandle,
    preview: Preview,
    chunk: usize,
    total: usize,
) {
    let Preview::ChunkProgress {
        note_id,
        recording_id,
    } = preview
    else {
        return;
    };
    let _ = app_handle.emit(
        NOTE_PROGRESS_EVENT,
        serde_json::json!({
            "noteId": note_id,
            "recordingId": recording_id,
            "chunk": chunk,
            "chunkTotal": total,
        }),
    );
}
