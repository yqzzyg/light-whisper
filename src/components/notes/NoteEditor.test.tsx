import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

const api = vi.hoisted(() => ({
  copyToClipboard: vi.fn(),
  saveTextFile: vi.fn(),
}));
vi.mock("@/api/tauri", () => api);

const toast = vi.hoisted(() => ({ success: vi.fn(), error: vi.fn() }));
vi.mock("sonner", () => ({ toast }));

vi.mock("react-i18next", () => ({
  useTranslation: () => ({
    i18n: { language: "zh" },
    t: (key: string) => key,
  }),
}));

// MarkdownContent 带数学公式预处理，这里只验证「预览态把内容渲染出来」。
vi.mock("@/components/MarkdownContent", () => ({
  default: ({ content }: { content: string }) => <div data-testid="markdown">{content}</div>,
}));

// 预估只是 19× 实时的除法，这里固定成 30 秒，断言「有没有显示预估」就够了。
vi.mock("@/hooks/noteTranscriptionStore", () => ({
  estimateTranscribeSeconds: (durationSec: number | null | undefined) =>
    durationSec ? 30 : null,
}));

import NoteEditor, {
  buildExportFileName,
  composeNoteExport,
} from "@/components/notes/NoteEditor";
import type { NoteDetail } from "@/types/notes";

function detail(overrides: Partial<NoteDetail> = {}): NoteDetail {
  return {
    id: 1,
    folderId: 3,
    title: "周会",
    content: "## 结论\n\n本周完成三件事",
    transcript: "大家好，我们开始。",
    summary: null,
    summaryModel: null,
    summaryGeneratedAt: null,
    summarySourceHash: null,
    kind: "meeting",
    durationSec: 600,
    historyId: null,
    audioFile: null,
    createdAt: 1,
    updatedAt: 1_700_000_000_000,
    ...overrides,
  };
}

function setup(overrides: Partial<React.ComponentProps<typeof NoteEditor>> = {}) {
  const props = {
    note: detail(),
    folders: [],
    loading: false,
    saving: false,
    untitled: "未命名笔记",
    recording: {
      active: false,
      sessionId: null,
      noteId: null,
      elapsedSec: 0,
      level: 0,
      audioFile: null,
      durationSec: null,
      error: null,
    },
    transcription: {
      noteId: null,
      running: false,
      elapsedSec: 0,
      error: null,
    },
    onStartRecording: vi.fn(),
    onTranscribe: vi.fn(),
    onEdit: vi.fn(),
    onFlush: vi.fn(),
    onMove: vi.fn(),
    onDelete: vi.fn(),
    onClose: vi.fn(),
    ...overrides,
  };
  render(<NoteEditor {...props} />);
  return props;
}

beforeEach(() => {
  api.copyToClipboard.mockReset().mockResolvedValue("");
  api.saveTextFile.mockReset().mockResolvedValue("/tmp/out.md");
  toast.success.mockReset();
  toast.error.mockReset();
});

describe("NoteEditor", () => {
  it("shows the empty hint without a note", () => {
    setup({ note: null });
    expect(screen.getByText("notes.selectHint")).toBeTruthy();
    expect(screen.queryByRole("tablist")).toBeNull();
  });

  it("edits the title through the store callback", () => {
    const props = setup();
    fireEvent.change(screen.getByPlaceholderText("未命名笔记"), { target: { value: "周会纪要" } });
    expect(props.onEdit).toHaveBeenCalledWith({ title: "周会纪要" });
  });

  it("switches between content, transcript and summary tabs", () => {
    setup();
    fireEvent.click(screen.getByRole("tab", { name: "notes.transcript" }));
    expect(screen.getByText("大家好，我们开始。")).toBeTruthy();

    fireEvent.click(screen.getByRole("tab", { name: "notes.summary" }));
    expect(screen.getByText("notes.summaryEmpty")).toBeTruthy();

    fireEvent.click(screen.getByRole("tab", { name: "notes.myNotes" }));
    expect(screen.getByPlaceholderText("notes.contentPlaceholder")).toBeTruthy();
  });

  it("renders the transcript empty state instead of a blank panel", () => {
    setup({ note: detail({ transcript: null }) });
    fireEvent.click(screen.getByRole("tab", { name: "notes.transcript" }));
    expect(screen.getByText("notes.transcriptEmpty")).toBeTruthy();
    expect(screen.getByText("notes.transcriptHint")).toBeTruthy();
  });

  it("points the user to the history record when the audio lives there", () => {
    setup({ note: detail({ historyId: 42, audioFile: null }) });
    fireEvent.click(screen.getByRole("tab", { name: "notes.transcript" }));
    expect(screen.getByText("notes.audioInHistory")).toBeTruthy();
  });

  it("does not point at history once the note owns its own recording", () => {
    setup({ note: detail({ historyId: 42, audioFile: "9-1700000000.wav" }) });
    fireEvent.click(screen.getByRole("tab", { name: "notes.transcript" }));
    expect(screen.queryByText("notes.audioInHistory")).toBeNull();
  });

  it("tells the user a recording exists but has not been transcribed", () => {
    setup({ note: detail({ audioFile: "note-9-1.wav", transcript: null, durationSec: 95 }) });
    const tab = screen.getByRole("tab", { name: "notes.transcript" });
    // 转录 tab 上挂告警色圆点：用户停在「我的笔记」也能看出还差一步。
    expect(tab.querySelector(".notes-tab-dot")?.getAttribute("data-pending")).toBe("true");

    fireEvent.click(tab);
    // i18n 被 mock 成返回 key，时长参数不参与断言。
    expect(screen.getByText("notes.recordedNotTranscribed")).toBeTruthy();
  });

  it("drops the not-transcribed hint once a transcript exists", () => {
    setup({ note: detail({ audioFile: "note-9-1.wav", transcript: "大家好" }) });
    const tab = screen.getByRole("tab", { name: "notes.transcript" });
    expect(tab.querySelector(".notes-tab-dot")?.getAttribute("data-pending")).toBe("false");

    fireEvent.click(tab);
    expect(screen.queryByText("notes.recordedNotTranscribed")).toBeNull();
  });

  it("offers the transcribe action for a recording without a transcript", () => {
    const props = setup({
      note: detail({ audioFile: "note-9-1.wav", transcript: null, durationSec: 95 }),
    });
    fireEvent.click(screen.getByRole("tab", { name: "notes.transcript" }));

    fireEvent.click(screen.getByRole("button", { name: /notes\.transcribe/ }));

    expect(props.onTranscribe).toHaveBeenCalledTimes(1);
  });

  it("shows elapsed time instead of the button while transcribing", () => {
    setup({
      note: detail({ audioFile: "note-9-1.wav", transcript: null, durationSec: 95 }),
      transcription: { noteId: 1, running: true, elapsedSec: 42, error: null },
    });
    fireEvent.click(screen.getByRole("tab", { name: "notes.transcript" }));

    expect(screen.getByText("notes.transcribing")).toBeTruthy();
    expect(screen.queryByRole("button", { name: /notes\.transcribe$/ })).toBeNull();
  });

  it("keeps the transcribe button disabled while another note is running", () => {
    setup({
      note: detail({ audioFile: "note-9-1.wav", transcript: null, durationSec: 95 }),
      transcription: { noteId: 99, running: true, elapsedSec: 5, error: null },
    });
    fireEvent.click(screen.getByRole("tab", { name: "notes.transcript" }));

    const button = screen.getByRole("button", { name: /notes\.transcribe/ });
    expect((button as HTMLButtonElement).disabled).toBe(true);
  });

  it("surfaces the failure message inline for the failed note", () => {
    setup({
      note: detail({ audioFile: "note-9-1.wav", transcript: null }),
      transcription: {
        noteId: null,
        running: false,
        elapsedSec: 0,
        error: { noteId: 1, message: "引擎未就绪" },
      },
    });
    fireEvent.click(screen.getByRole("tab", { name: "notes.transcript" }));

    expect(screen.getByText("notes.transcribeFailed")).toBeTruthy();
  });

  it("starts a recording from the header", () => {
    const props = setup();
    fireEvent.click(screen.getByRole("button", { name: "notes.recordStart" }));
    expect(props.onStartRecording).toHaveBeenCalledTimes(1);
  });

  it("disables the record button while a recording is already running", () => {
    setup({
      recording: {
        active: true,
        sessionId: 1,
        noteId: 9,
        elapsedSec: 3,
        level: 0.2,
        audioFile: null,
        durationSec: null,
        error: null,
      },
    });
    const button = screen.getByRole("button", { name: "notes.recordStart" });
    expect((button as HTMLButtonElement).disabled).toBe(true);
  });

  it("toggles markdown preview and mounts the renderer only there", () => {
    setup();
    expect(screen.queryByTestId("markdown")).toBeNull();

    fireEvent.click(screen.getByText("notes.preview"));
    expect(screen.getByTestId("markdown").textContent).toContain("本周完成三件事");

    fireEvent.click(screen.getByText("notes.edit"));
    expect(screen.queryByTestId("markdown")).toBeNull();
  });

  it("copies the transcript and reports success", async () => {
    setup();
    fireEvent.click(screen.getByRole("tab", { name: "notes.transcript" }));
    fireEvent.click(screen.getByText("notes.copy"));

    await waitFor(() => expect(api.copyToClipboard).toHaveBeenCalledWith("大家好，我们开始。"));
    expect(toast.success).toHaveBeenCalledWith("notes.copied");
  });

  it("exports the transcript as plain text", async () => {
    setup();
    fireEvent.click(screen.getByRole("tab", { name: "notes.transcript" }));
    fireEvent.click(screen.getByText("notes.exportTxt"));

    await waitFor(() => expect(api.saveTextFile).toHaveBeenCalled());
    const [fileName, content] = api.saveTextFile.mock.calls[0];
    expect(fileName).toBe("周会.txt");
    expect(content).toContain("周会");
    expect(content).toContain("大家好，我们开始。");
    expect(content).not.toContain("# 周会");
  });

  it("keeps quiet when the save dialog is cancelled", async () => {
    api.saveTextFile.mockResolvedValue(null);
    setup({ note: detail({ summary: "要点" }) });
    fireEvent.click(screen.getByRole("tab", { name: "notes.summary" }));
    fireEvent.click(screen.getByText("notes.exportMarkdown"));

    await waitFor(() => expect(api.saveTextFile).toHaveBeenCalled());
    expect(toast.success).not.toHaveBeenCalled();
  });

  it("surfaces export failures through a toast", async () => {
    api.saveTextFile.mockRejectedValue(new Error("磁盘只读"));
    setup();
    fireEvent.click(screen.getByRole("tab", { name: "notes.transcript" }));
    fireEvent.click(screen.getByText("notes.exportTxt"));

    await waitFor(() =>
      expect(toast.error).toHaveBeenCalledWith("notes.exportFailed"),
    );
  });
});

describe("composeNoteExport", () => {
  const labels = { transcript: "转录", summary: "摘要" };

  it("writes title, body, transcript and summary as markdown sections", () => {
    const text = composeNoteExport(
      detail({ summary: "## 要点" }),
      "markdown",
      labels,
    );
    expect(text).toBe(
      "# 周会\n\n## 结论\n\n本周完成三件事\n\n## 转录\n\n大家好，我们开始。\n\n## 摘要\n\n## 要点\n",
    );
  });

  it("skips absent sections and blank titles", () => {
    const text = composeNoteExport(
      detail({ title: "   ", content: "只有正文", transcript: null, summary: null }),
      "text",
      labels,
    );
    expect(text).toBe("只有正文\n");
  });

  it("labels sections in plain text without markdown markers", () => {
    const text = composeNoteExport(detail({ content: "" }), "text", labels);
    expect(text).toBe("周会\n\n转录\n\n大家好，我们开始。\n");
  });
});

describe("buildExportFileName", () => {
  it("replaces path separators and windows-invalid characters", () => {
    expect(buildExportFileName('周会/纪要: "1"', "md")).toBe("周会 纪要 1.md");
  });

  it("falls back to note when the title has nothing usable", () => {
    expect(buildExportFileName("   ", "txt")).toBe("note.txt");
    expect(buildExportFileName("...", "md")).toBe("note.md");
  });

  it("caps very long titles", () => {
    const name = buildExportFileName("长".repeat(200), "md");
    expect(name.length).toBe(60 + 3);
  });
});
