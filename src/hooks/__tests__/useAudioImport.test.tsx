import { act, renderHook, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => {
  // 简易事件总线：listen 注册 handler，emit 手动触发。
  const handlers = new Set<(event: { payload: Record<string, unknown> }) => void>();
  return {
    pickAudioFile: vi.fn(),
    transcribeImportedAudio: vi.fn(),
    toastSuccess: vi.fn(),
    toastError: vi.fn(),
    t: vi.fn((key: string, vars?: Record<string, unknown>) =>
      vars ? `${key}:${JSON.stringify(vars)}` : key,
    ),
    listen: vi.fn((_event: string, handler: (event: { payload: Record<string, unknown> }) => void) => {
      handlers.add(handler);
      return Promise.resolve(() => handlers.delete(handler));
    }),
    emitInterim: (payload: Record<string, unknown>) => {
      handlers.forEach((handler) => handler({ payload }));
    },
    handlerCount: () => handlers.size,
    clearHandlers: () => handlers.clear(),
  };
});

vi.mock("@tauri-apps/api/event", () => ({
  listen: mocks.listen,
}));

vi.mock("@/api/tauri", () => ({
  pickAudioFile: mocks.pickAudioFile,
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

describe("useAudioImport", () => {
  beforeEach(() => {
    // 状态在模块级，用例之间必须显式重置。
    resetAudioImportStoreForTests();
    vi.clearAllMocks();
    mocks.pickAudioFile.mockResolvedValue("C:/audio/test.wav");
    mocks.transcribeImportedAudio.mockResolvedValue(successRecord());
  });

  afterEach(() => {
    resetAudioImportStoreForTests();
    vi.clearAllMocks();
    mocks.clearHandlers();
  });

  it("cancels silently when the user dismisses the picker", async () => {
    mocks.pickAudioFile.mockResolvedValue(null);
    const { result } = renderHook(() => useAudioImport());

    await act(async () => {
      await result.current.importAudio();
    });

    expect(mocks.transcribeImportedAudio).not.toHaveBeenCalled();
    expect(mocks.toastSuccess).not.toHaveBeenCalled();
    expect(mocks.toastError).not.toHaveBeenCalled();
    expect(result.current.isImporting).toBe(false);
  });

  it("keeps the transcribed result for preview instead of a success toast", async () => {
    const { result } = renderHook(() => useAudioImport());

    await act(async () => {
      await result.current.importAudio();
    });

    expect(mocks.transcribeImportedAudio).toHaveBeenCalledWith("C:/audio/test.wav");
    expect(result.current.result).toEqual(successRecord());
    // 结果已在上传页预览，不再用 toast 重复告知。
    expect(mocks.toastSuccess).not.toHaveBeenCalled();
    expect(result.current.isImporting).toBe(false);
  });

  it("clears the previous result as soon as a new import starts", async () => {
    const { result } = renderHook(() => useAudioImport());
    await act(async () => {
      await result.current.importAudio();
    });
    expect(result.current.result).not.toBeNull();

    let resolveTranscribe!: (value: unknown) => void;
    mocks.transcribeImportedAudio.mockImplementation(
      () => new Promise((resolve) => { resolveTranscribe = resolve; }),
    );
    let pending!: Promise<void>;
    act(() => {
      pending = result.current.importAudio();
    });
    await waitFor(() => expect(result.current.result).toBeNull());
    await act(async () => {
      resolveTranscribe(successRecord({ id: 2, text: "新结果" }));
      await pending;
    });
    expect(result.current.result?.text).toBe("新结果");
  });

  it("shows an error toast when the record is asr_error", async () => {
    mocks.transcribeImportedAudio.mockResolvedValue(
      successRecord({ status: "asr_error", error: "音频过长" }),
    );
    const { result } = renderHook(() => useAudioImport());

    await act(async () => {
      await result.current.importAudio();
    });

    expect(mocks.toastError).toHaveBeenCalled();
    expect(mocks.toastSuccess).not.toHaveBeenCalled();
    expect(result.current.result).toBeNull();
  });

  it("shows an error toast when the command rejects", async () => {
    mocks.transcribeImportedAudio.mockRejectedValue(new Error("解码失败"));
    const { result } = renderHook(() => useAudioImport());

    await act(async () => {
      await result.current.importAudio();
    });

    expect(mocks.toastError).toHaveBeenCalled();
    await waitFor(() => expect(result.current.isImporting).toBe(false));
  });

  it("ignores concurrent clicks while importing", async () => {
    let resolveTranscribe!: (value: unknown) => void;
    mocks.transcribeImportedAudio.mockImplementation(
      () => new Promise((resolve) => { resolveTranscribe = resolve; }),
    );
    const { result } = renderHook(() => useAudioImport());

    let first!: Promise<void>;
    let second!: Promise<void>;
    act(() => {
      first = result.current.importAudio();
      second = result.current.importAudio();
    });
    // 等第一个调用真正进入 transcribe（pickAudioFile 是异步的）。
    await waitFor(() => {
      expect(mocks.transcribeImportedAudio).toHaveBeenCalledTimes(1);
      expect(resolveTranscribe).toBeTypeOf("function");
    });
    await act(async () => {
      resolveTranscribe(successRecord());
      await Promise.all([first, second]);
    });

    expect(mocks.transcribeImportedAudio).toHaveBeenCalledTimes(1);
  });

  it("exposes negative-session interim text during streaming and clears it on finish", async () => {
    let resolveTranscribe!: (value: unknown) => void;
    mocks.transcribeImportedAudio.mockImplementation(
      () => new Promise((resolve) => { resolveTranscribe = resolve; }),
    );
    const { result } = renderHook(() => useAudioImport());

    let pending!: Promise<void>;
    act(() => {
      pending = result.current.importAudio();
    });
    await waitFor(() => expect(mocks.transcribeImportedAudio).toHaveBeenCalledTimes(1));
    await waitFor(() => expect(mocks.handlerCount()).toBeGreaterThan(0));

    // 负数 sessionId 的 interim 应被接收；正数（录音会话）应被忽略。
    act(() => {
      mocks.emitInterim({ sessionId: 5, text: "录音字幕", interim: true });
      mocks.emitInterim({ sessionId: -1_000_001, text: "导入的字", interim: true });
      mocks.emitInterim({ sessionId: -1_000_002, text: "导入的字更多", interim: true });
    });
    expect(result.current.interimText).toBe("导入的字更多");

    await act(async () => {
      resolveTranscribe(successRecord({ text: "导入的字更多" }));
      await pending;
    });
    expect(result.current.interimText).toBeNull();
    expect(result.current.result?.text).toBe("导入的字更多");
  });

  it("keeps the result after the page unmounts and mounts again", async () => {
    const first = renderHook(() => useAudioImport());
    await act(async () => {
      await first.result.current.importAudio();
    });
    expect(first.result.current.result?.text).toBe("你好");

    // 模拟左侧栏切走：只有当前页面挂载，UploadPage 会被卸载。
    first.unmount();

    const second = renderHook(() => useAudioImport());
    expect(second.result.current.result?.text).toBe("你好");
    expect(second.result.current.isImporting).toBe(false);
  });

  it("finishes the import even if the page unmounts mid-way", async () => {
    let resolveTranscribe!: (value: unknown) => void;
    mocks.transcribeImportedAudio.mockImplementation(
      () => new Promise((resolve) => { resolveTranscribe = resolve; }),
    );
    const first = renderHook(() => useAudioImport());

    let pending!: Promise<void>;
    act(() => {
      pending = first.result.current.importAudio();
    });
    await waitFor(() => expect(mocks.transcribeImportedAudio).toHaveBeenCalledTimes(1));

    first.unmount();
    await act(async () => {
      resolveTranscribe(successRecord({ id: 9, text: "离开页面后才出的结果" }));
      await pending;
    });

    const second = renderHook(() => useAudioImport());
    expect(second.result.current.result?.text).toBe("离开页面后才出的结果");
    expect(second.result.current.isImporting).toBe(false);
  });
});
