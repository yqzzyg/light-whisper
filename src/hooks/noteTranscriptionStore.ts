import { useSyncExternalStore } from "react";

import {
  transcribeNoteRecording,
  transcribeNoteRecordingSegment,
} from "@/api/tauri";
import { reloadOpenNote, refreshNotes } from "@/hooks/notesStore";

/**
 * 本地引擎实测约 19 倍实时。
 *
 * 只用来给「大概还要多久」的预估，不做假进度条：整段转写是引擎内的黑盒，
 * 前端唯一确定的信息就是已经等了多久。
 */
export const TRANSCRIBE_REALTIME_FACTOR = 19;

/** 预估下限：再短的录音也要几秒，显示「0 秒」反而像已经卡住。 */
const MIN_ESTIMATE_SECONDS = 5;

export interface NoteTranscriptionError {
  noteId: number;
  message: string;
}

export interface NoteTranscriptionState {
  /** 正在转写的笔记；空闲时为 `null`。 */
  noteId: number | null;
  /** 正在转写的录音段；整篇转录或空闲时为 `null`。 */
  recordingId: number | null;
  running: boolean;
  /** 从开始到现在过了多少秒，由本地计时器推进（后端没有进度事件）。 */
  elapsedSec: number;
  /** 上一次失败；留在状态里，切回来还能看到原因与重试入口。 */
  error: NoteTranscriptionError | null;
}

const IDLE: NoteTranscriptionState = {
  noteId: null,
  recordingId: null,
  running: false,
  elapsedSec: 0,
  error: null,
};

/**
 * 转写状态放模块级：整段转写可能几分钟，用户切走页面时 NotesPage 会卸载，
 * 但后端命令仍在跑，状态必须跟着命令走而不是跟着组件走。
 */
let state: NoteTranscriptionState = IDLE;
const listeners = new Set<() => void>();

let startedAt = 0;
let ticker: ReturnType<typeof setInterval> | null = null;

function emit(patch: Partial<NoteTranscriptionState>) {
  state = { ...state, ...patch };
  for (const listener of listeners) listener();
}

function stopTicker() {
  if (ticker !== null) {
    clearInterval(ticker);
    ticker = null;
  }
}

function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  return String(error);
}

/** 按录音时长给一个预估秒数；时长未知（老数据）时返回 `null`，界面就不显示预估。 */
export function estimateTranscribeSeconds(
  durationSec: number | null | undefined,
): number | null {
  if (!durationSec || !Number.isFinite(durationSec) || durationSec <= 0) return null;
  return Math.max(MIN_ESTIMATE_SECONDS, Math.round(durationSec / TRANSCRIBE_REALTIME_FACTOR));
}

export function subscribeNoteTranscription(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

export function getNoteTranscriptionSnapshot(): NoteTranscriptionState {
  return state;
}

export function useNoteTranscription(): NoteTranscriptionState {
  return useSyncExternalStore(
    subscribeNoteTranscription,
    getNoteTranscriptionSnapshot,
    getNoteTranscriptionSnapshot,
  );
}

/**
 * 转写一篇笔记的录音。返回错误文案（成功为 `null`），由调用方决定是否弹 toast。
 *
 * 已有转写在跑时直接返回 `null` 而不是报错：按钮本身是禁用的，能走到这里只能是
 * 双击的竞态，没必要给用户看一句「正在转写」。
 */
export async function transcribeNote(noteId: number): Promise<string | null> {
  return runTranscription(noteId, null, () => transcribeNoteRecording(noteId));
}

/**
 * 只重转一段录音。段级状态会记下 recordingId，界面据此只在该段显示进度。
 * 已转写的段也允许重跑（后端不做幂等），挑出识别不佳的单段重来。
 */
export async function transcribeSegment(
  noteId: number,
  recordingId: number,
): Promise<string | null> {
  return runTranscription(noteId, recordingId, () =>
    transcribeNoteRecordingSegment(recordingId),
  );
}

async function runTranscription(
  noteId: number,
  recordingId: number | null,
  invoke: () => Promise<unknown>,
): Promise<string | null> {
  if (state.running) return null;

  startedAt = Date.now();
  stopTicker();
  emit({ noteId, recordingId, running: true, elapsedSec: 0, error: null });
  ticker = setInterval(() => {
    emit({ elapsedSec: Math.floor((Date.now() - startedAt) / 1000) });
  }, 1000);

  try {
    await invoke();
    // 转录与 history_id 由后端写库：打开中的笔记与侧栏摘要都要重读。
    void reloadOpenNote();
    void refreshNotes(true);
    return null;
  } catch (error) {
    const message = errorMessage(error);
    emit({ error: { noteId, message } });
    return message;
  } finally {
    stopTicker();
    emit({ noteId: null, recordingId: null, running: false, elapsedSec: 0 });
  }
}

/** 只给测试用：清空模块级状态与计时器。 */
export function resetNoteTranscriptionForTests() {
  stopTicker();
  startedAt = 0;
  state = IDLE;
  for (const listener of listeners) listener();
}
