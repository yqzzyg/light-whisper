import { useCallback, useSyncExternalStore } from "react";
import { useTranslation } from "react-i18next";

import {
  getAudioImportSnapshot,
  startAudioImport,
  subscribeAudioImport,
  type AudioImportState,
} from "./audioImportStore";

export interface UseAudioImportReturn extends AudioImportState {
  importAudio: () => Promise<void>;
}

/**
 * 导入音频转写：选择文件 → 转写 → 写入历史，并把结果留在上传页预览。
 *
 * 本地引擎边转边推 interim（负数 sessionId），只接收负数 sessionId 的中间结果
 * ——浮层与录音主界面都会忽略它们，导入字幕不会串到别处。
 * 在线引擎不支持流式，转完一次性出结果。结果不回灌主结果区、不自动粘贴。
 *
 * 状态存放在模块级 store（见 audioImportStore.ts），左侧栏切走再回来不会丢失，
 * 转写在离开页面期间也会继续跑完。
 */
export function useAudioImport(): UseAudioImportReturn {
  const { t } = useTranslation();
  const state = useSyncExternalStore(
    subscribeAudioImport,
    getAudioImportSnapshot,
    getAudioImportSnapshot,
  );
  const importAudio = useCallback(() => startAudioImport(t), [t]);

  return { ...state, importAudio };
}
