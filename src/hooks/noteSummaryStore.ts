import { useSyncExternalStore } from "react";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

import { cancelNoteSummary, generateNoteSummary } from "@/api/tauri";
import { reloadOpenNote, refreshNotes } from "@/hooks/notesStore";

/** 后端 `note-summary-status` 事件的状态位。 */
export type NoteSummaryPhase =
  | "preparing"
  | "part"
  | "merging"
  | "streaming";

export interface NoteSummaryState {
  /** 正在生成摘要的笔记；空闲时为 `null`。 */
  noteId: number | null;
  running: boolean;
  phase: NoteSummaryPhase | null;
  /** 分块进度：第几块 / 共几块（单块素材时 total 为 0）。 */
  part: number;
  total: number;
  /** 合并阶段的流式增量，用于边生成边预览。 */
  preview: string;
  /** 上一次失败；留在状态里，切回来仍能看到原因。 */
  error: { noteId: number; message: string } | null;
}

const IDLE: NoteSummaryState = {
  noteId: null,
  running: false,
  phase: null,
  part: 0,
  total: 0,
  preview: "",
  error: null,
};

/** 后端把「用户主动取消」标成这个错误码（`AppError::Cancelled`）。 */
const CANCELLED_CODE = "CANCELLED";

/**
 * 摘要状态放模块级：长会议要分块跑几分钟，用户切走页面时组件会卸载，
 * 但后端命令仍在跑，状态必须跟着命令走而不是跟着组件走。
 */
let state: NoteSummaryState = IDLE;
const listeners = new Set<() => void>();

let unsubscribe: UnlistenFn | null = null;
let subscribing: Promise<void> | null = null;
let subscribeFailed = false;

function emit(patch: Partial<NoteSummaryState>) {
  state = { ...state, ...patch };
  for (const listener of listeners) listener();
}

function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  return String(error);
}

/**
 * 按错误码判断「用户取消」。
 *
 * 不用 `instanceof IpcError`：部分单测会把 `@/api/tauri` 整体 mock 成只有命令函数，
 * 类身份在那儿不存在；而 `invokeCommand` 归一化后真正稳定的契约是 `code`。
 */
function isCancelledError(error: unknown): boolean {
  return (
    typeof error === "object" &&
    error !== null &&
    (error as { code?: unknown }).code === CANCELLED_CODE
  );
}

interface SummaryEvent {
  noteId?: number;
  status?: string;
  part?: number;
  total?: number;
  chunk?: string;
  message?: string;
}

export function subscribeNoteSummary(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

export function getNoteSummarySnapshot(): NoteSummaryState {
  return state;
}

export function useNoteSummary(): NoteSummaryState {
  return useSyncExternalStore(
    subscribeNoteSummary,
    getNoteSummarySnapshot,
    getNoteSummarySnapshot,
  );
}

/**
 * 订阅后端进度事件（只订阅一次）。
 *
 * 懒订阅而不是模块加载时订阅：测试环境没有 Tauri IPC，模块顶层 listen 会直接
 * 抛错；界面上也只在打开笔记页时才需要。
 */
export async function ensureNoteSummarySubscribed(): Promise<void> {
  if (subscribeFailed) return;
  // `unsubscribe` 才是「已订阅」的标志：`subscribing` 在完成后会被置回 null，
  // 只判它的话，每次重新挂载（切走设置页再回来）都会多注册一个监听，
  // 流式 chunk 会被重复追加，预览文字出现重复。
  if (unsubscribe) return;
  if (!subscribing) {
    subscribing = listen<SummaryEvent>("note-summary-status", (event) => {
      const payload = event.payload;
      if (!payload) return;

      // 流式 chunk 由 llm_client 统一构造，只带 sessionId（这里没有传），
      // 所以不能按 noteId 过滤；同一时刻只允许一个摘要任务，running 期间
      // 收到的 chunk 一定是当前这条笔记的。
      if (payload.status === "streaming") {
        if (state.noteId === null) return;
        emit({ phase: "streaming", preview: state.preview + (payload.chunk ?? "") });
        return;
      }

      // 其余状态由本模块自己发，带 noteId：切走时另一篇的进度不该污染这一篇。
      if (state.noteId === null || payload.noteId !== state.noteId) return;
      switch (payload.status) {
        case "preparing":
          emit({ phase: "preparing", part: 0, total: 0, preview: "" });
          break;
        case "part":
          emit({
            phase: "part",
            part: payload.part ?? 0,
            total: payload.total ?? 0,
            preview: "",
          });
          break;
        case "merging":
          emit({
            phase: "merging",
            part: payload.total ?? 0,
            total: payload.total ?? 0,
            preview: "",
          });
          break;
        default:
          // done / failed / cancelled 由命令的 promise 收尾，这里不重复处理。
          break;
      }
    })
      .then((stop) => {
        unsubscribe = stop;
      })
      .catch((error) => {
        subscribeFailed = true;
        console.error("订阅摘要进度失败:", error);
      })
      .finally(() => {
        subscribing = null;
      });
  }
  await subscribing;
}

/**
 * 生成（或重新生成）一篇笔记的摘要。返回错误文案（成功或取消为 `null`）。
 */
export async function generateSummary(noteId: number): Promise<string | null> {
  if (state.running) return null;

  emit({
    noteId,
    running: true,
    phase: "preparing",
    part: 0,
    total: 0,
    preview: "",
    error: null,
  });

  try {
    await generateNoteSummary(noteId);
    // 摘要由后端写库：打开中的笔记与侧栏的「有摘要」标记都要重读。
    void reloadOpenNote();
    void refreshNotes(true);
    return null;
  } catch (error) {
    // 用户主动取消不是错误，不弹红字。
    if (isCancelledError(error)) return null;
    const message = errorMessage(error);
    emit({ error: { noteId, message } });
    return message;
  } finally {
    emit({
      noteId: null,
      running: false,
      phase: null,
      part: 0,
      total: 0,
      preview: "",
    });
  }
}

/** 请求取消；取消结果仍由 `generateSummary` 的 promise 收尾。 */
export async function cancelSummary(): Promise<void> {
  try {
    await cancelNoteSummary();
  } catch (error) {
    console.error("取消摘要生成失败:", error);
  }
}

/** 只给测试用：清空模块级状态与订阅。 */
export function resetNoteSummaryForTests() {
  if (unsubscribe) {
    unsubscribe();
    unsubscribe = null;
  }
  subscribeFailed = false;
  subscribing = null;
  state = IDLE;
  for (const listener of listeners) listener();
}
