import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  selectAudio: vi.fn(),
  startImport: vi.fn(),
  cancelSelection: vi.fn(),
  isImporting: false,
  picked: null as { path: string; probe: Record<string, unknown> } | null,
  result: null as Record<string, unknown> | null,
  hideMainWindow: vi.fn(),
  minimize: vi.fn(),
  copyToClipboard: vi.fn(),
  createNoteFromHistory: vi.fn(),
  listNoteFolders: vi.fn(),
  listNotes: vi.fn(),
  toastSuccess: vi.fn(),
  toastError: vi.fn(),
}));

vi.mock("@/hooks/useAudioImport", () => ({
  useAudioImport: () => ({
    selectAudio: mocks.selectAudio,
    startImport: mocks.startImport,
    cancelSelection: mocks.cancelSelection,
    isImporting: mocks.isImporting,
    picked: mocks.picked,
    result: mocks.result,
  }),
}));

vi.mock("@/api/tauri", () => ({
  hideMainWindow: mocks.hideMainWindow,
  copyToClipboard: mocks.copyToClipboard,
  createNoteFromHistory: mocks.createNoteFromHistory,
  listNoteFolders: mocks.listNoteFolders,
  listNotes: mocks.listNotes,
}));
vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({ minimize: mocks.minimize }),
}));
vi.mock("@/components/TitleBar", () => ({
  default: ({ title }: { title: string }) => <header>{title}</header>,
}));
vi.mock("sonner", () => ({
  toast: { success: mocks.toastSuccess, error: mocks.toastError },
}));
vi.mock("react-i18next", () => ({
  useTranslation: () => ({
    t: (key: string, vars?: Record<string, unknown>) =>
      vars ? `${key}:${JSON.stringify(vars)}` : key,
  }),
}));

import UploadPage from "@/pages/UploadPage";

function successRecord(overrides: Record<string, unknown> = {}) {
  return {
    id: 1,
    sessionId: 123,
    createdAt: 1,
    updatedAt: 1,
    mode: "import",
    workflow: "import",
    status: "success",
    text: "识别结果文本",
    originalText: "识别结果文本",
    engine: "qwen3-asr-0.6b",
    audioAvailable: true,
    ...overrides,
  };
}

function pickedAudio(probeOverrides: Record<string, unknown> = {}) {
  return {
    path: "C:/audio/会议录音.mp3",
    probe: {
      durationSec: 317,
      sizeBytes: 5 * 1024 * 1024,
      sampleRate: 44100,
      engine: "qwen3-asr-0.6b",
      limitHint: null,
      estimatedSecs: 80,
      ...probeOverrides,
    },
  };
}

describe("UploadPage", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    mocks.isImporting = false;
    mocks.picked = null;
    mocks.result = null;
  });

  it("renders the upload title and select button", () => {
    render(<UploadPage onNavigate={vi.fn()} />);
    expect(screen.getByText("upload.title")).toBeTruthy();
    expect(screen.getByRole("button", { name: /upload.selectFile/ })).toBeTruthy();
  });

  it("selects a file without starting the transcription", () => {
    render(<UploadPage onNavigate={vi.fn()} />);
    fireEvent.click(screen.getByRole("button", { name: /upload.selectFile/ }));
    expect(mocks.selectAudio).toHaveBeenCalledTimes(1);
    expect(mocks.startImport).not.toHaveBeenCalled();
  });

  it("shows the picked file's duration, size and estimate before transcribing", () => {
    mocks.picked = pickedAudio();
    render(<UploadPage onNavigate={vi.fn()} />);

    expect(screen.getByText("会议录音.mp3")).toBeTruthy();
    expect(screen.getByText("5:17")).toBeTruthy();
    expect(screen.getByText("5.0 MB")).toBeTruthy();
    expect(screen.getByText(/upload.estimateMinutes/)).toBeTruthy();
    expect(screen.getByText("upload.pickedTitle")).toBeTruthy();
    // 选完不能自动开始：必须点「开始转写」。
    expect(mocks.startImport).not.toHaveBeenCalled();
  });

  it("starts the transcription from the confirm step", () => {
    mocks.picked = pickedAudio();
    render(<UploadPage onNavigate={vi.fn()} />);
    fireEvent.click(screen.getByRole("button", { name: /upload.startTranscribe/ }));
    expect(mocks.startImport).toHaveBeenCalledTimes(1);
  });

  it("blocks the start button when the engine cannot handle the file", () => {
    mocks.picked = pickedAudio({ limitHint: "超过 GLM-ASR 的 30 秒上限" });
    render(<UploadPage onNavigate={vi.fn()} />);

    const button = screen.getByRole("button", { name: /upload.startTranscribe/ }) as HTMLButtonElement;
    expect(button.disabled).toBe(true);
    expect(screen.getByText("超过 GLM-ASR 的 30 秒上限")).toBeTruthy();
    fireEvent.click(button);
    expect(mocks.startImport).not.toHaveBeenCalled();
  });

  it("drops the picked file from the confirm step", () => {
    mocks.picked = pickedAudio();
    render(<UploadPage onNavigate={vi.fn()} />);
    fireEvent.click(screen.getByRole("button", { name: "upload.chooseAnother" }));
    expect(mocks.cancelSelection).toHaveBeenCalledTimes(1);
  });

  it("disables the button and shows importing state while importing", () => {
    mocks.isImporting = true;
    render(<UploadPage onNavigate={vi.fn()} />);
    const button = screen.getByRole("button", { name: /upload.importing/ }) as HTMLButtonElement;
    expect(button.disabled).toBe(true);
    fireEvent.click(button);
    expect(mocks.selectAudio).not.toHaveBeenCalled();
  });

  it("does not show a result preview before any import", () => {
    render(<UploadPage onNavigate={vi.fn()} />);
    expect(screen.queryByText("upload.resultHint")).toBeNull();
    expect(screen.queryByRole("textbox")).toBeNull();
  });

  it("previews the transcribed result in the same result card as the home page", () => {
    mocks.result = successRecord({ text: "第一行\n第二行" });
    render(<UploadPage onNavigate={vi.fn()} />);
    const textbox = screen.getByRole("textbox", {
      name: "result.editableTranscription",
    }) as HTMLTextAreaElement;
    expect(textbox.value).toBe("第一行\n第二行");
    expect(screen.getByText("upload.resultHint")).toBeTruthy();
    expect(screen.getByRole("button", { name: /upload.importAnother/ })).toBeTruthy();
  });

  it("saves the imported result as a note and opens the notes page", async () => {
    mocks.result = successRecord({ text: "会议原文" });
    mocks.createNoteFromHistory.mockResolvedValue({ id: 1, folderId: null });
    mocks.listNoteFolders.mockResolvedValue([]);
    mocks.listNotes.mockResolvedValue({ items: [], total: 0, hasMore: false });
    const onNavigate = vi.fn();
    render(<UploadPage onNavigate={onNavigate} />);

    fireEvent.click(screen.getByRole("button", { name: "upload.saveToNotes" }));

    await waitFor(() => expect(mocks.createNoteFromHistory).toHaveBeenCalledWith(1));
    expect(mocks.toastSuccess).toHaveBeenCalledWith("upload.saveToNotesDone");
    expect(onNavigate).toHaveBeenCalledWith("notes");
  });

  it("copies the previewed result text", async () => {
    mocks.result = successRecord({ text: "可复制文本" });
    mocks.copyToClipboard.mockResolvedValue("ok");
    render(<UploadPage onNavigate={vi.fn()} />);
    fireEvent.click(screen.getByRole("button", { name: "common.copy" }));
    await waitFor(() => {
      expect(mocks.copyToClipboard).toHaveBeenCalledWith("可复制文本");
    });
    expect(mocks.toastSuccess).toHaveBeenCalledWith("common.copiedToClipboard");
  });

  it("starts another selection from the result state", () => {
    mocks.result = successRecord();
    render(<UploadPage onNavigate={vi.fn()} />);
    fireEvent.click(screen.getByRole("button", { name: /upload.importAnother/ }));
    expect(mocks.selectAudio).toHaveBeenCalledTimes(1);
  });

  it("applies the animation class to the page root", () => {
    const { container } = render(<UploadPage onNavigate={vi.fn()} animClass="page-enter-right" />);
    expect(container.querySelector(".upload-page")?.classList.contains("page-enter-right")).toBe(true);
  });
});
