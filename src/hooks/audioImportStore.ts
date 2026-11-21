import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type { TFunction } from "i18next";
import { toast } from "sonner";

import { pickAudioFile, probeAudioFile, transcribeImportedAudio } from "@/api/tauri";
import type { AudioProbe, PersistentHistoryRecord } from "@/types";

interface ImportInterimPayload {
  sessionId: number;
  text: string;
  interim?: boolean;
}

/** 已选中、尚未开始转写的文件。 */
export interface PickedAudio {
  path: string;
  probe: AudioProbe;
}

export interface AudioImportState {
  /** 正在转写。 */
  isImporting: boolean;
  /** 已选文件与元信息；用户点「开始转写」前一直留在这里。 */
  picked: PickedAudio | null;
  /** 最近一次成功转写的记录，供上传页直接预览；失败或新一轮选择时清空。 */
  result: PersistentHistoryRecord | null;
  /** 本地引擎流式转写过程中的实时文本；仅在导入中非空。 */
  interimText: string | null;
}

const INITIAL_STATE: AudioImportState = {
  isImporting: false,
  picked: null,
  result: null,
  interimText: null,
};

/**
 * 导入状态放在模块级而不是组件内。
 *
 * 左侧栏切换时只挂载当前页面，UploadPage 会卸载；如果状态跟着组件走，
 * 选好的文件、转写结果（以及正在进行的转写）都会随卸载丢失。放在模块级后，
 * 组件只做订阅，离开页面再回来仍能看到结果，转写过程也不会中断。
 */
let state: AudioImportState = INITIAL_STATE;
const listeners = new Set<() => void>();

/** 选择与转写互斥：两次点击最多只有一个在跑。 */
let busy = false;
let unlisten: UnlistenFn | null = null;
let subscribing: Promise<void> | null = null;

function setState(patch: Partial<AudioImportState>) {
  const next = { ...state, ...patch };
  if (
    next.isImporting === state.isImporting
    && next.picked === state.picked
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

/**
 * 选择音频并探测信息，不发起转写。
 *
 * 选完文件立刻能拿到时长与预估耗时，用户可以判断要不要等——比点了按钮等两分钟
 * 才发现文件选错要好。探测失败（格式不支持、文件损坏）在这里就报错，不进入下一步。
 */
export async function selectAudio(t: TFunction): Promise<void> {
  if (busy) return;
  busy = true;
  try {
    const path = await pickAudioFile();
    if (!path) return; // 用户取消：保留当前已选文件与上一次结果。

    const probe = await probeAudioFile(path);
    setState({ picked: { path, probe }, result: null, interimText: null });
  } catch (error) {
    toast.error(t("toast.importAudioFailed", { error: errorMessage(error) }));
  } finally {
    busy = false;
  }
}

/** 放弃已选文件，回到「选择音频文件」。 */
export function cancelSelection(): void {
  if (busy) return;
  setState({ picked: null });
}

/**
 * 转写已选中的文件。
 *
 * 本地引擎边转边推 interim（负数 sessionId），只接收负数 sessionId 的中间结果
 * ——浮层与录音主界面都会忽略它们，导入字幕不会串到别处。在线引擎不支持流式，
 * 转完一次性出结果。结果不回灌主结果区、不自动粘贴。
 */
export async function startImport(t: TFunction): Promise<void> {
  const picked = state.picked;
  if (busy || !picked) return;
  if (picked.probe.limitHint) {
    toast.error(t("toast.importAudioFailed", { error: picked.probe.limitHint }));
    return;
  }

  busy = true;
  setState({ isImporting: true, result: null, interimText: null });
  try {
    await ensureInterimSubscription();
    const record: PersistentHistoryRecord = await transcribeImportedAudio(picked.path);
    if (record.status === "asr_error") {
      // 保留 picked：同一份文件可以直接重试，不必重新选一遍。
      toast.error(t("toast.importAudioFailed", { error: record.error || t("common.retry") }));
      return;
    }
    // 结果直接呈现在上传页，不再用 toast 重复告知。
    setState({ result: record, picked: null });
  } catch (error) {
    toast.error(t("toast.importAudioFailed", { error: errorMessage(error) }));
  } finally {
    releaseInterimSubscription();
    busy = false;
    setState({ isImporting: false, interimText: null });
  }
}

/** 仅用于测试：清空模块级状态，避免用例之间互相影响。 */
export function resetAudioImportStoreForTests() {
  releaseInterimSubscription();
  busy = false;
  state = INITIAL_STATE;
  listeners.clear();
}
