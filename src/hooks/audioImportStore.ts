import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type { TFunction } from "i18next";
import { toast } from "sonner";

import { pickAudioFile, transcribeImportedAudio } from "@/api/tauri";
import type { PersistentHistoryRecord } from "@/types";

interface ImportInterimPayload {
  sessionId: number;
  text: string;
  interim?: boolean;
}

export interface AudioImportState {
  isImporting: boolean;
  /** 最近一次成功转写的记录，供上传页直接预览；失败或新一轮导入时清空。 */
  result: PersistentHistoryRecord | null;
  /** 本地引擎流式转写过程中的实时文本；仅在导入中非空。 */
  interimText: string | null;
}

const INITIAL_STATE: AudioImportState = {
  isImporting: false,
  result: null,
  interimText: null,
};

/**
 * 导入状态放在模块级而不是组件内。
 *
 * 左侧栏切换时只挂载当前页面，UploadPage 会卸载；如果状态跟着组件走，
 * 转写结果（以及正在进行的转写）都会随卸载丢失。放在模块级后，组件只做订阅，
 * 离开页面再回来仍能看到结果，转写过程也不会中断。
 */
let state: AudioImportState = INITIAL_STATE;
const listeners = new Set<() => void>();

let inFlight = false;
let unlisten: UnlistenFn | null = null;
let subscribing: Promise<void> | null = null;

function setState(patch: Partial<AudioImportState>) {
  const next = { ...state, ...patch };
  if (
    next.isImporting === state.isImporting
    && next.result === state.result
    && next.interimText === state.interimText
  ) {
    return;
  }
  state = next;
  for (const listener of listeners) listener();
}

export function subscribeAudioImport(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

export function getAudioImportSnapshot(): AudioImportState {
  return state;
}

// 订阅流式中间结果。导入是串行的，且全应用只有导入会发负数 sessionId，
// 因此任何负数 interim 都属于当前这一轮，无需再按 sessionId 精确匹配。
// 订阅挂在模块上，不随页面卸载取消；仅在导入期间生效。
function ensureInterimSubscription(): Promise<void> {
  if (unlisten) return Promise.resolve();
  if (!subscribing) {
    subscribing = listen<ImportInterimPayload>("transcription-result", (event) => {
      if (!state.isImporting) return;
      const payload = event.payload;
      const sessionId = Number(payload.sessionId);
      if (!payload.interim || !Number.isFinite(sessionId) || sessionId >= 0) return;
      if (typeof payload.text === "string" && payload.text) {
        setState({ interimText: payload.text });
      }
    })
      .then((stop) => {
        unlisten = stop;
      })
      .catch((error) => {
        // 订阅失败不应阻断导入（用户仍可等转完从历史查看）。
        console.error("Failed to subscribe import interim events:", error);
      })
      .finally(() => {
        subscribing = null;
      });
  }
  return subscribing;
}

function releaseInterimSubscription() {
  if (!unlisten) return;
  unlisten();
  unlisten = null;
}

function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  return String(error);
}

export async function startAudioImport(t: TFunction): Promise<void> {
  // 防止重复点击并发导入。
  if (inFlight) return;
  inFlight = true;
  setState({ isImporting: true });

  try {
    const path = await pickAudioFile();
    if (!path) return; // 用户取消，保留上一次的结果。

    // 新一轮转写开始，先撤掉上一次的结果与字幕，避免展示过期内容。
    setState({ result: null, interimText: null });
    await ensureInterimSubscription();

    const record: PersistentHistoryRecord = await transcribeImportedAudio(path);
    if (record.status === "asr_error") {
      toast.error(t("toast.importAudioFailed", { error: record.error || t("common.retry") }));
      return;
    }
    // 结果直接呈现在上传页，不再用 toast 重复告知。
    setState({ result: record });
  } catch (error) {
    toast.error(t("toast.importAudioFailed", { error: errorMessage(error) }));
  } finally {
    releaseInterimSubscription();
    inFlight = false;
    setState({ isImporting: false, interimText: null });
  }
}

/** 仅用于测试：清空模块级状态，避免用例之间互相影响。 */
export function resetAudioImportStoreForTests() {
  releaseInterimSubscription();
  inFlight = false;
  state = INITIAL_STATE;
  listeners.clear();
}
