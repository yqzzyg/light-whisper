import { beforeEach, describe, expect, it, vi } from "vitest";

const api = vi.hoisted(() => ({
  generateNoteSummary: vi.fn(),
  cancelNoteSummary: vi.fn(),
}));
vi.mock("@/api/tauri", () => api);

const notes = vi.hoisted(() => ({
  refreshNotes: vi.fn(async () => {}),
  reloadOpenNote: vi.fn(async () => {}),
}));
vi.mock("@/hooks/notesStore", () => notes);

const events = vi.hoisted(() => ({
  handlers: [] as Array<(event: { payload: unknown }) => void>,
  unlisten: vi.fn(),
}));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async (_name: string, handler: (event: { payload: unknown }) => void) => {
    events.handlers.push(handler);
    return events.unlisten;
  }),
}));

import {
  cancelSummary,
  ensureNoteSummarySubscribed,
  generateSummary,
  getNoteSummarySnapshot,
  resetNoteSummaryForTests,
} from "@/hooks/noteSummaryStore";

function emit(payload: unknown) {
  for (const handler of events.handlers) handler({ payload });
}

beforeEach(() => {
  resetNoteSummaryForTests();
  events.handlers = [];
  events.unlisten.mockClear();
  api.generateNoteSummary.mockReset();
  api.cancelNoteSummary.mockReset().mockResolvedValue(undefined);
  notes.refreshNotes.mockClear();
  notes.reloadOpenNote.mockClear();
});

describe("noteSummaryStore", () => {
  it("starts idle", () => {
    const snapshot = getNoteSummarySnapshot();
    expect(snapshot.running).toBe(false);
    expect(snapshot.noteId).toBeNull();
    expect(snapshot.phase).toBeNull();
    expect(snapshot.error).toBeNull();
  });

  it("tracks part progress and streaming preview for the active note", async () => {
    let finish!: (value: unknown) => void;
    api.generateNoteSummary.mockReturnValue(
      new Promise((resolve) => {
        finish = resolve;
      }),
    );
    await ensureNoteSummarySubscribed();

    const running = generateSummary(7);
    await vi.waitFor(() => expect(getNoteSummarySnapshot().running).toBe(true));

    emit({ noteId: 7, status: "part", part: 2, total: 3 });
    expect(getNoteSummarySnapshot()).toMatchObject({ phase: "part", part: 2, total: 3 });

    emit({ noteId: 7, status: "merging", total: 3 });
    expect(getNoteSummarySnapshot().phase).toBe("merging");

    emit({ noteId: 7, status: "streaming", chunk: "## 概要\n" });
    emit({ noteId: 7, status: "streaming", chunk: "结论" });
    expect(getNoteSummarySnapshot().preview).toBe("## 概要\n结论");

    finish(null);
    await running;
    expect(getNoteSummarySnapshot()).toMatchObject({
      running: false,
      noteId: null,
      preview: "",
    });
  });

  it("ignores progress events from another note", async () => {
    let finish!: (value: unknown) => void;
    api.generateNoteSummary.mockReturnValue(
      new Promise((resolve) => {
        finish = resolve;
      }),
    );
    await ensureNoteSummarySubscribed();

    const running = generateSummary(7);
    await vi.waitFor(() => expect(getNoteSummarySnapshot().running).toBe(true));

    emit({ noteId: 8, status: "part", part: 1, total: 2 });
    expect(getNoteSummarySnapshot().part).toBe(0);

    finish(null);
    await running;
  });

  it("ignores streaming chunks when no summary is running", async () => {
    await ensureNoteSummarySubscribed();
    // 流式 payload 只带 chunk、没有 noteId：空闲时必须丢掉，不能残留预览。
    emit({ status: "streaming", chunk: "迟到的片段" });
    expect(getNoteSummarySnapshot().preview).toBe("");
  });

  it("refreshes the open note after a successful run", async () => {
    api.generateNoteSummary.mockResolvedValue({ id: 7 });

    await expect(generateSummary(7)).resolves.toBeNull();
    expect(notes.reloadOpenNote).toHaveBeenCalled();
    expect(notes.refreshNotes).toHaveBeenCalledWith(true);
  });

  it("keeps the failure message for the note that failed", async () => {
    api.generateNoteSummary.mockRejectedValue(new Error("没有可用的 AI 凭据"));

    await expect(generateSummary(7)).resolves.toBe("没有可用的 AI 凭据");
    expect(getNoteSummarySnapshot().error).toEqual({
      noteId: 7,
      message: "没有可用的 AI 凭据",
    });
    expect(getNoteSummarySnapshot().running).toBe(false);
  });

  it("treats a cancelled run as quiet success", async () => {
    // 契约是错误码：后端 AppError::Cancelled 序列化成 code=CANCELLED。
    api.generateNoteSummary.mockRejectedValue({
      code: "CANCELLED",
      category: "cancelled",
      message: "操作已取消",
    });

    await expect(generateSummary(7)).resolves.toBeNull();
    expect(getNoteSummarySnapshot().error).toBeNull();
  });

  it("does not treat a matching message as cancellation without the code", async () => {
    // 判定只看 code：文案随时可能改，靠截图里的句子比对迟早会错。
    api.generateNoteSummary.mockRejectedValue(new Error("已取消生成"));

    await expect(generateSummary(7)).resolves.toBe("已取消生成");
    expect(getNoteSummarySnapshot().error).toEqual({
      noteId: 7,
      message: "已取消生成",
    });
  });

  it("forwards cancel requests to the backend", async () => {
    await cancelSummary();
    expect(api.cancelNoteSummary).toHaveBeenCalled();
  });

  it("subscribes once even when the editor remounts", async () => {
    // 切走设置页再回到笔记页会重新挂载并再次调用；`subscribing` 完成后已置回
    // null，守卫必须看 `unsubscribe`，否则每回来一次就多一个监听，
    // 流式 chunk 被重复追加，预览文字重复。
    await ensureNoteSummarySubscribed();
    await ensureNoteSummarySubscribed();
    await ensureNoteSummarySubscribed();

    expect(events.handlers).toHaveLength(1);
  });
});
