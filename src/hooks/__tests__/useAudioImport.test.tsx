import { act, renderHook, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  pickAudioFile: vi.fn(),
  probeAudioFile: vi.fn(),
  transcribeImportedAudio: vi.fn(),
  toastSuccess: vi.fn(),
  toastError: vi.fn(),
  t: vi.fn((key: string, vars?: Record<string, unknown>) =>
    vars ? `${key}:${JSON.stringify(vars)}` : key,
  ),
}));

vi.mock("@/api/tauri", () => ({
  pickAudioFile: mocks.pickAudioFile,
  probeAudioFile: mocks.probeAudioFile,
  transcribeImportedAudio: mocks.transcribeImportedAudio,
}));

vi.mock("sonner", () => ({
  toast: {
    success: mocks.toastSuccess,
    error: mocks.toastError,
  },
}));

vi.mock("react-i18next", () => ({
  useTranslation: () => ({ t: mocks.t }),
}));

import { useAudioImport } from "../useAudioImport";
import { resetAudioImportStoreForTests } from "../audioImportStore";

function successRecord(overrides: Record<string, unknown> = {}) {
  return {
    id: 1,
    sessionId: 123,
    createdAt: 1,
    updatedAt: 1,
    mode: "import",
    workflow: "import",
    status: "success",
    text: "你好",
    originalText: "你好",
    engine: "qwen3-asr-0.6b",
    audioAvailable: true,
    ...overrides,
  };
}

/** 后端 `transcribe_imported_audio` 的返回形状：历史记录 + 分段 + 降级原因。 */
function transcriptionResponse(
  overrides: Record<string, unknown> = {},
  segments: unknown[] = [],
  fallbackReason: string | null = null,
) {
  return { record: successRecord(overrides), segments, fallbackReason };
}

function probe(overrides: Record<string, unknown> = {}) {  return {
    durationSec: 317,
    sizeBytes: 5 * 1024 * 1024,
    sampleRate: 44100,
    engine: "qwen3-asr-0.6b",
    limitHint: null,
    ...overrides,
  };
}

type Hook = { current: ReturnType<typeof useAudioImport> };

/** 走完「选文件 → 探测 → 开始转写」两步。 */
async function pickAndStart(result: Hook) {
  await act(async () => {
    await result.current.selectAudio();
  });
  await act(async () => {
    await result.current.startImport();
  });
}

describe("useAudioImport", () => {
  beforeEach(() => {
    // 状态在模块级，用例之间必须显式重置。
    resetAudioImportStoreForTests();
    vi.clearAllMocks();
    mocks.pickAudioFile.mockResolvedValue("C:/audio/test.wav");
    mocks.probeAudioFile.mockResolvedValue(probe());
    mocks.transcribeImportedAudio.mockResolvedValue(transcriptionResponse());
  });

  afterEach(() => {
    resetAudioImportStoreForTests();
    vi.clearAllMocks();
  });

  it("only probes on selection and waits for an explicit start", async () => {
    const { result } = renderHook(() => useAudioImport());

    await act(async () => {
      await result.current.selectAudio();
    });

    expect(mocks.probeAudioFile).toHaveBeenCalledWith("C:/audio/test.wav");
    expect(result.current.picked).toEqual({
      path: "C:/audio/test.wav",
      probe: probe(),
    });
    // 选完文件不自动转写：用户要先确认文件信息。
    expect(mocks.transcribeImportedAudio).not.toHaveBeenCalled();
    expect(result.current.isImporting).toBe(false);
  });

  it("cancels silently when the user dismisses the picker", async () => {
    mocks.pickAudioFile.mockResolvedValue(null);
    const { result } = renderHook(() => useAudioImport());

    await act(async () => {
      await result.current.selectAudio();
    });

    expect(mocks.probeAudioFile).not.toHaveBeenCalled();
    expect(result.current.picked).toBeNull();
    expect(mocks.toastError).not.toHaveBeenCalled();
  });

  it("reports a probe failure without entering the confirm step", async () => {
    mocks.probeAudioFile.mockRejectedValue(new Error("音频格式探测失败"));
    const { result } = renderHook(() => useAudioImport());

    await act(async () => {
      await result.current.selectAudio();
    });

    expect(mocks.toastError).toHaveBeenCalled();
    expect(result.current.picked).toBeNull();
  });

  it("refuses to start when the engine cannot handle the file", async () => {
    mocks.probeAudioFile.mockResolvedValue(
      probe({ limitHint: "音频约 317 秒，超过 GLM-ASR 的 30 秒上限" }),
    );
    const { result } = renderHook(() => useAudioImport());

    await act(async () => {
      await result.current.selectAudio();
    });
    await act(async () => {
      await result.current.startImport();
    });

    // 拦截文案在 UI 里已经显示，这里不再发请求。
    expect(mocks.transcribeImportedAudio).not.toHaveBeenCalled();
    expect(result.current.isImporting).toBe(false);
  });

  it("drops the selection when the user chooses another file", async () => {
    const { result } = renderHook(() => useAudioImport());

    await act(async () => {
      await result.current.selectAudio();
    });
    act(() => {
      result.current.cancelSelection();
    });

    expect(result.current.picked).toBeNull();
  });

  it("keeps the transcribed result for preview instead of a success toast", async () => {
    const { result } = renderHook(() => useAudioImport());

    await pickAndStart(result);

    expect(mocks.transcribeImportedAudio).toHaveBeenCalledWith("C:/audio/test.wav");
    expect(result.current.result).toEqual(successRecord());
    // 结果已在上传页预览，不再用 toast 重复告知。
    expect(mocks.toastSuccess).not.toHaveBeenCalled();
    // 转完就收起选中态，卡片回到「再传一个」。
    expect(result.current.picked).toBeNull();
    expect(result.current.isImporting).toBe(false);
  });

  it("clears the previous result as soon as a new import starts", async () => {
    const { result } = renderHook(() => useAudioImport());
    await pickAndStart(result);
    expect(result.current.result).not.toBeNull();

    let resolveTranscribe!: (value: unknown) => void;
    mocks.transcribeImportedAudio.mockImplementation(
      () => new Promise((resolve) => { resolveTranscribe = resolve; }),
    );
    await act(async () => {
      await result.current.selectAudio();
    });
    let pending!: Promise<void>;
    act(() => {
      pending = result.current.startImport();
    });
    await waitFor(() => expect(result.current.result).toBeNull());
    await act(async () => {
      resolveTranscribe(transcriptionResponse({ id: 2, text: "新结果" }));
      await pending;
    });
    expect(result.current.result?.text).toBe("新结果");
  });

  it("shows an error toast when the record is asr_error and keeps the selection", async () => {
    mocks.transcribeImportedAudio.mockResolvedValue(
      transcriptionResponse({ status: "asr_error", error: "音频过长" }),
    );
    const { result } = renderHook(() => useAudioImport());

    await pickAndStart(result);

    expect(mocks.toastError).toHaveBeenCalled();
    expect(mocks.toastSuccess).not.toHaveBeenCalled();
    expect(result.current.result).toBeNull();
    // 同一份文件可以直接重试，不必重新选。
    expect(result.current.picked).not.toBeNull();
  });

  it("shows an error toast when the command rejects", async () => {
    mocks.transcribeImportedAudio.mockRejectedValue(new Error("解码失败"));
    const { result } = renderHook(() => useAudioImport());

    await pickAndStart(result);

    expect(mocks.toastError).toHaveBeenCalled();
    await waitFor(() => expect(result.current.isImporting).toBe(false));
  });

  it("ignores a second start while the first is running", async () => {
    let resolveTranscribe!: (value: unknown) => void;
    mocks.transcribeImportedAudio.mockImplementation(
      () => new Promise((resolve) => { resolveTranscribe = resolve; }),
    );
    const { result } = renderHook(() => useAudioImport());
    await act(async () => {
      await result.current.selectAudio();
    });

    let first!: Promise<void>;
    let second!: Promise<void>;
    act(() => {
      first = result.current.startImport();
      second = result.current.startImport();
    });
    await waitFor(() => expect(mocks.transcribeImportedAudio).toHaveBeenCalledTimes(1));
    await act(async () => {
      resolveTranscribe(successRecord());
      await Promise.all([first, second]);
    });

    expect(mocks.transcribeImportedAudio).toHaveBeenCalledTimes(1);
  });

  it("keeps the result after the page unmounts and mounts again", async () => {
    const first = renderHook(() => useAudioImport());
    await pickAndStart(first.result);
    expect(first.result.current.result?.text).toBe("你好");

    // 模拟左侧栏切走：只有当前页面挂载，UploadPage 会被卸载。
    first.unmount();

    const second = renderHook(() => useAudioImport());
    expect(second.result.current.result?.text).toBe("你好");
    expect(second.result.current.isImporting).toBe(false);
  });

  it("keeps the picked file when the page unmounts before starting", async () => {
    const first = renderHook(() => useAudioImport());
    await act(async () => {
      await first.result.current.selectAudio();
    });
    first.unmount();

    const second = renderHook(() => useAudioImport());
    expect(second.result.current.picked?.path).toBe("C:/audio/test.wav");
  });

  it("finishes the import even if the page unmounts mid-way", async () => {
    let resolveTranscribe!: (value: unknown) => void;
    mocks.transcribeImportedAudio.mockImplementation(
      () => new Promise((resolve) => { resolveTranscribe = resolve; }),
    );
    const first = renderHook(() => useAudioImport());
    await act(async () => {
      await first.result.current.selectAudio();
    });

    let pending!: Promise<void>;
    act(() => {
      pending = first.result.current.startImport();
    });
    await waitFor(() => expect(mocks.transcribeImportedAudio).toHaveBeenCalledTimes(1));

    first.unmount();
    await act(async () => {
      resolveTranscribe(transcriptionResponse({ id: 9, text: "离开页面后才出的结果" }));
      await pending;
    });

    const second = renderHook(() => useAudioImport());
    expect(second.result.current.result?.text).toBe("离开页面后才出的结果");
    expect(second.result.current.isImporting).toBe(false);
  });

  it("keeps the speaker segments and the fallback reason of the same response", async () => {
    mocks.transcribeImportedAudio.mockResolvedValue(
      transcriptionResponse(
        {},
        [
          { speakerId: 0, startMs: 0, endMs: 2_000, text: "你好", overlap: false },
          { speakerId: 1, startMs: 2_000, endMs: 4_000, text: "在的", overlap: true },
        ],
        "models_missing",
      ),
    );
    const { result } = renderHook(() => useAudioImport());

    await pickAndStart(result);

    expect(result.current.segments).toHaveLength(2);
    expect(result.current.segments[1].speakerId).toBe(1);
    expect(result.current.fallbackReason).toBe("models_missing");
  });

  it("drops the previous segments as soon as a new import starts", async () => {
    mocks.transcribeImportedAudio.mockResolvedValue(
      transcriptionResponse({}, [
        { speakerId: 0, startMs: 0, endMs: 2_000, text: "上一份", overlap: false },
      ]),
    );
    const { result } = renderHook(() => useAudioImport());
    await pickAndStart(result);
    expect(result.current.segments).toHaveLength(1);

    let resolveTranscribe!: (value: unknown) => void;
    mocks.transcribeImportedAudio.mockImplementation(
      () => new Promise((resolve) => { resolveTranscribe = resolve; }),
    );
    await act(async () => {
      await result.current.selectAudio();
    });
    let pending!: Promise<void>;
    act(() => {
      pending = result.current.startImport();
    });

    // 转写一开始就要清空，否则新结果出来前会短暂显示上一份的分段。
    await waitFor(() => expect(result.current.segments).toHaveLength(0));
    await act(async () => {
      resolveTranscribe(transcriptionResponse({ id: 2 }));
      await pending;
    });
  });
});
