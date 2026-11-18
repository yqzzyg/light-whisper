//! 会议录音命令。
//!
//! 与 `notes.rs` 一样是薄壳：会话、互斥、落盘都在 `note_recording_service`，
//! 命令层只负责把 AppHandle/State 递进去，避免两条路径各维护一套规则。

use crate::services::note_recording_service::{self, NoteRecordingSnapshot};
use crate::state::AppState;

#[tauri::command]
pub async fn start_note_recording(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    note_id: i64,
) -> Result<NoteRecordingSnapshot, String> {
    note_recording_service::start(&app, state.inner(), note_id).await
}

/// 停止录音并把文件名、时长写回笔记。
#[tauri::command]
pub async fn stop_note_recording(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<NoteRecordingSnapshot, String> {
    note_recording_service::stop(&app, state.inner()).await
}

/// 取消录音：删除半成品文件，笔记不动。
#[tauri::command]
pub async fn cancel_note_recording(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    note_recording_service::cancel(&app, state.inner()).await
}

#[tauri::command]
pub fn get_note_recording_state(state: tauri::State<'_, AppState>) -> NoteRecordingSnapshot {
    note_recording_service::snapshot(state.inner())
}
