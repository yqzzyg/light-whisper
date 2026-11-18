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
