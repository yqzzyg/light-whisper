import type { TFunction } from "i18next";
import { toast } from "sonner";

import { pickAudioFile, probeAudioFile, transcribeImportedAudio } from "@/api/tauri";
import type { AudioProbe, PersistentHistoryRecord } from "@/types";

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
}

const INITIAL_STATE: AudioImportState = {
  isImporting: false,
  picked: null,
  result: null,
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

function setState(patch: Partial<AudioImportState>) {
  const next = { ...state, ...patch };
  if (
    next.isImporting === state.isImporting
    && next.picked === state.picked
    && next.result === state.result
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

function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  return String(error);
}

/**
 * 选择音频并探测信息，不发起转写。
 *
 * 选完文件立刻能拿到时长，用户可以确认自己选对了文件；探测失败（格式不支持、
 * 文件损坏）在这里就报错，不进入下一步。
 */
export async function selectAudio(t: TFunction): Promise<void> {
  if (busy) return;
  busy = true;
  try {
    const path = await pickAudioFile();
    if (!path) return; // 用户取消：保留当前已选文件与上一次结果。

    const probe = await probeAudioFile(path);
    setState({ picked: { path, probe }, result: null });
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
 * 转写期间没有逐字文本：长音频走切片、短音频整段，页面只显示进度或等待状态。
 * 结果不回灌主结果区、不自动粘贴。
 */
export async function startImport(t: TFunction): Promise<void> {
  const picked = state.picked;
  if (busy || !picked) return;
  if (picked.probe.limitHint) {
    toast.error(t("toast.importAudioFailed", { error: picked.probe.limitHint }));
    return;
  }

  busy = true;
  setState({ isImporting: true, result: null });
  try {
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
    busy = false;
    setState({ isImporting: false });
  }
}

/** 仅用于测试：清空模块级状态，避免用例之间互相影响。 */
export function resetAudioImportStoreForTests() {
  busy = false;
  state = INITIAL_STATE;
  listeners.clear();
}
