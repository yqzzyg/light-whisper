import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { useSyncExternalStore } from "react";

import {
  cancelNoteRecording,
  getNoteRecordingState,
  startNoteRecording,
  stopNoteRecording,
} from "@/api/tauri";
import { reloadOpenNote, refreshNotes } from "@/hooks/notesStore";
import type { NoteRecordingSnapshot } from "@/types";

const IDLE: NoteRecordingSnapshot = {
  active: false,
  sessionId: null,
  noteId: null,
  elapsedSec: 0,
  level: 0,
  audioFile: null,
  durationSec: null,
  error: null,
};

/**
 * 录音状态放模块级：切换页面会让 NotesPage 卸载，但录音必须继续。
 *
 * 后端每 500ms 推一次 `note-recording-state`，停止/取消时最后一条 `active`
 * 为 false，所以前端不需要自己维护计时器，也不会和真实落盘进度漂移。
 */
let state: NoteRecordingSnapshot = IDLE;
const listeners = new Set<() => void>();

let unlisten: UnlistenFn | null = null;
let subscribing: Promise<void> | null = null;
let actionInFlight = false;

function emit(next: NoteRecordingSnapshot) {
  state = next;
  for (const listener of listeners) listener();
}

export function subscribeNoteRecording(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

export function getNoteRecordingSnapshot(): NoteRecordingSnapshot {
  return state;
}

export function useNoteRecording(): NoteRecordingSnapshot {
  return useSyncExternalStore(
    subscribeNoteRecording,
    getNoteRecordingSnapshot,
    getNoteRecordingSnapshot,
  );
}

function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  return String(error);
}

/** 停止后刷新笔记与列表：`audioFile`/`durationSec` 由后端写回，前端必须重读。 */
function refreshAfterStop(snapshot: NoteRecordingSnapshot) {
  void reloadOpenNote();
  void refreshNotes(true);
  if (snapshot.error) {
    console.error("会议录音写入失败:", snapshot.error);
  }
}

/**
 * 订阅事件并同步一次当前状态。
 *
 * 订阅失败不抛给调用方（用户仍能靠 `get_state` 的返回值工作），但要留日志，
 * 否则「计时不动」这类问题会无从下手。
 */
export async function ensureNoteRecordingReady(): Promise<void> {
  if (!subscribing) {
    subscribing = listen<NoteRecordingSnapshot>("note-recording-state", (event) => {
      const payload = event.payload;
      if (!payload || typeof payload.active !== "boolean") return;
      emit(payload);
      if (!payload.active && payload.noteId != null) {
        refreshAfterStop(payload);
      }
    })
      .then((stop) => {
        unlisten = stop;
      })
      .catch((error) => {
        console.error("订阅会议录音状态失败:", error);
      })
      .finally(() => {
        subscribing = null;
      });
  }
  await subscribing;
  if (actionInFlight) return;
  try {
    emit(await getNoteRecordingState());
  } catch (error) {
    console.error("读取会议录音状态失败:", error);
  }
}

export function releaseNoteRecordingSubscription() {
  if (!unlisten) return;
  unlisten();
  unlisten = null;
}

/** 开始录音。返回错误文案（成功为 `null`），由调用方决定怎么提示。 */
export async function startRecording(noteId: number): Promise<string | null> {
  if (actionInFlight) return null;
  actionInFlight = true;
  try {
    emit(await startNoteRecording(noteId));
    return null;
  } catch (error) {
    return errorMessage(error);
  } finally {
    actionInFlight = false;
  }
}

/** 停止录音并按后端结果刷新笔记。返回错误文案（成功为 `null`）。 */
export async function stopRecording(): Promise<string | null> {
  if (actionInFlight) return null;
  actionInFlight = true;
  try {
    const snapshot = await stopNoteRecording();
    emit(snapshot);
    refreshAfterStop(snapshot);
    return snapshot.error;
  } catch (error) {
    return errorMessage(error);
  } finally {
    actionInFlight = false;
  }
}

/** 取消录音：丢弃半成品，笔记不动。返回错误文案（成功为 `null`）。 */
export async function cancelRecording(): Promise<string | null> {
  if (actionInFlight) return null;
  actionInFlight = true;
  try {
    await cancelNoteRecording();
    emit(IDLE);
    return null;
  } catch (error) {
    return errorMessage(error);
  } finally {
    actionInFlight = false;
  }
}

/** 只有测试用：清空模块级状态与订阅。 */
export function resetNoteRecordingForTests() {
  releaseNoteRecordingSubscription();
  state = IDLE;
  actionInFlight = false;
}
