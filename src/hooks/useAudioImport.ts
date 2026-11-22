import { useCallback, useSyncExternalStore } from "react";
import { useTranslation } from "react-i18next";

import {
  cancelSelection as cancelSelectionInStore,
  getAudioImportSnapshot,
  selectAudio as selectAudioInStore,
  startImport as startImportInStore,
  subscribeAudioImport,
  type AudioImportState,
} from "./audioImportStore";

export interface UseAudioImportReturn extends AudioImportState {
  /** 选择文件并读取时长等信息，不发起转写。 */
  selectAudio: () => Promise<void>;
  /** 转写已选中的文件。 */
  startImport: () => Promise<void>;
  cancelSelection: () => void;
}

/**
 * 导入音频转写：选择文件 → 确认信息 → 转写 → 写入历史，结果留在上传页预览。
 *
 * 分两步是有意的：选完文件先显示文件信息，用户可以决定现在转还是换一个，
 * 不必点一下等两分钟才发现选错文件。
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
  const selectAudio = useCallback(() => selectAudioInStore(t), [t]);
  const startImport = useCallback(() => startImportInStore(t), [t]);
  const cancelSelection = useCallback(() => cancelSelectionInStore(), []);

  return { ...state, selectAudio, startImport, cancelSelection };
}
