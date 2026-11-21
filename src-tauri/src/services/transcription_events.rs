//! 转写过程的 UI 事件。
//!
//! [`Preview`] 只表达调用方的意图；事件名与 payload 形状收在这里，ASR 分派层不必
//! 知道前端有几个页面、哪些事件有人听。

use tauri::Emitter;

/// 转写过程中要推送的预览。
///
/// 上传页曾经有「逐段实时文本」（`ImportInterim`），2026-11-21 改为只显示进度：
/// 渐进文字要求原生流式，而流式在 Rust 与 Python 两侧各留一份整段 PCM，
/// 1 小时音频峰值约 660MiB。现在上传与笔记用同一份切片进度事件。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preview {
    /// 不推送任何进度。
    None,
    /// 笔记：切片转写到第几块。
    ChunkProgress { note_id: i64, recording_id: i64 },
}

/// 切片进度事件名。
pub const NOTE_PROGRESS_EVENT: &str = "note-transcription-progress";

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
