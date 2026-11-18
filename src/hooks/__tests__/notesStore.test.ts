import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const api = vi.hoisted(() => ({
  listNoteFolders: vi.fn(),
  createNoteFolder: vi.fn(),
  renameNoteFolder: vi.fn(),
  deleteNoteFolder: vi.fn(),
  reorderNoteFolders: vi.fn(),
  listNotes: vi.fn(),
  getNote: vi.fn(),
  createNote: vi.fn(),
  updateNote: vi.fn(),
  deleteNote: vi.fn(),
  moveNote: vi.fn(),
  createNoteFromHistory: vi.fn(),
}));
vi.mock("@/api/tauri", () => api);

import {
  createNoteFromHistory,
  editNote,
  ensureNotesLoaded,
  flushNoteSave,
  getNotesSnapshot,
  openNote,
  removeNote,
  resetNotesStoreForTests,
  setActiveFolder,
  toggleFolder,
} from "@/hooks/notesStore";
import type { NoteDetail, NoteFolder, NoteListItem, NotePage } from "@/types/notes";

const folder: NoteFolder = {
  id: 3,
  name: "会议",
  sortOrder: 0,
  noteCount: 2,
  createdAt: 1,
  updatedAt: 1,
};

function listItem(id: number, title: string): NoteListItem {
  return {
    id,
    folderId: 3,
    title,
    kind: "personal",
    excerpt: "",
    durationSec: null,
    hasTranscript: false,
    hasSummary: false,
    hasAudio: false,
    createdAt: 1,
    updatedAt: 1,
  };
}

function detail(id: number, title: string): NoteDetail {
  return {
    id,
    folderId: 3,
    title,
    content: "",
    transcript: null,
    summary: null,
    summaryModel: null,
    summaryGeneratedAt: null,
    summarySourceHash: null,
    kind: "personal",
    durationSec: null,
    historyId: null,
    audioFile: null,
    createdAt: 1,
    updatedAt: 1,
  };
}

const page = (items: NoteListItem[]): NotePage => ({ items, total: items.length, hasMore: false });

beforeEach(() => {
  vi.useFakeTimers();
  for (const mock of Object.values(api)) mock.mockReset();
  api.listNoteFolders.mockResolvedValue([folder]);
  api.listNotes.mockResolvedValue(page([listItem(1, "周会记录")]));
  api.getNote.mockResolvedValue(detail(1, "周会记录"));
  api.updateNote.mockImplementation(async (id: number, patch: { title?: string; content?: string }) => ({
    ...detail(id, patch.title ?? "周会记录"),
  }));
});

afterEach(() => {
  resetNotesStoreForTests();
  vi.useRealTimers();
});

describe("notesStore", () => {
  it("loads folders once but refreshes the list on every entry", async () => {
    await ensureNotesLoaded();
    await ensureNotesLoaded();

    expect(api.listNoteFolders).toHaveBeenCalledTimes(1);
    expect(api.listNotes).toHaveBeenCalledTimes(2);
    expect(getNotesSnapshot().folders).toHaveLength(1);
    expect(getNotesSnapshot().notes[0].title).toBe("周会记录");
    expect(getNotesSnapshot().foldersLoaded).toBe(true);
  });

  it("always loads the whole list so the sidebar can group it by folder", async () => {
    await ensureNotesLoaded();
    setActiveFolder(folder.id);

    // 目录只是高亮与新建笔记的落点，不参与查询，数据量才不会随目录切换反复请求。
    expect(api.listNotes).toHaveBeenCalledTimes(1);
    expect(api.listNotes).toHaveBeenLastCalledWith({ query: "", limit: 200, offset: 0 });
    expect(getNotesSnapshot().activeFolderId).toBe(folder.id);
  });

  it("toggles a folder without touching the data", async () => {
    await ensureNotesLoaded();
    toggleFolder(folder.id);
    expect(getNotesSnapshot().collapsedFolderIds).toEqual([folder.id]);

    // 选中目录会顺手展开它，避免「点了没反应」
    setActiveFolder(folder.id);
    expect(getNotesSnapshot().collapsedFolderIds).toEqual([]);

    toggleFolder(folder.id);
    toggleFolder(folder.id);
    expect(getNotesSnapshot().collapsedFolderIds).toEqual([]);
    expect(api.listNotes).toHaveBeenCalledTimes(1);
  });

  it("shows edits immediately and persists them after the debounce", async () => {
    await ensureNotesLoaded();
    await openNote(1);

    editNote({ title: "新标题" });
    expect(getNotesSnapshot().note?.title).toBe("新标题");
    expect(getNotesSnapshot().saving).toBe(true);
    expect(api.updateNote).not.toHaveBeenCalled();

    await vi.advanceTimersByTimeAsync(600);

    expect(api.updateNote).toHaveBeenCalledWith(1, { title: "新标题" });
    expect(getNotesSnapshot().saving).toBe(false);
    // 列表项同步更新，切回列表时不会看到旧标题
    expect(getNotesSnapshot().notes[0].title).toBe("新标题");
  });

  it("merges consecutive edits into one request", async () => {
    await ensureNotesLoaded();
    await openNote(1);

    editNote({ title: "标" });
    await vi.advanceTimersByTimeAsync(300);
    editNote({ title: "标题", content: "正文" });
    await vi.advanceTimersByTimeAsync(600);

    expect(api.updateNote).toHaveBeenCalledTimes(1);
    expect(api.updateNote).toHaveBeenCalledWith(1, { title: "标题", content: "正文" });
  });

  it("flushes the pending save before switching notes", async () => {
    await ensureNotesLoaded();
    await openNote(1);
    editNote({ content: "还没落库的内容" });

    await openNote(1);

    expect(api.updateNote).toHaveBeenCalledWith(1, { content: "还没落库的内容" });
    expect(getNotesSnapshot().note?.content).toBe("");
  });

  it("keeps the patch queued when the save fails so the next flush retries", async () => {
    await ensureNotesLoaded();
    await openNote(1);
    api.updateNote.mockRejectedValueOnce(new Error("磁盘只读"));

    editNote({ title: "会失败的标题" });
    await vi.advanceTimersByTimeAsync(600);
    expect(getNotesSnapshot().saving).toBe(false);
    expect(getNotesSnapshot().error).toContain("磁盘只读");

    await flushNoteSave();
    expect(api.updateNote).toHaveBeenCalledTimes(2);
    expect(api.updateNote).toHaveBeenLastCalledWith(1, { title: "会失败的标题" });
  });

  it("clears the selection after deleting the open note", async () => {
    await ensureNotesLoaded();
    await openNote(1);
    api.deleteNote.mockResolvedValue(null);

    await removeNote(1);

    expect(getNotesSnapshot().selectedNoteId).toBeNull();
    expect(getNotesSnapshot().note).toBeNull();
    expect(api.listNotes).toHaveBeenCalledTimes(2);
  });

  it("opens the note created from a history record and refreshes the list", async () => {
    await ensureNotesLoaded();
    api.createNoteFromHistory.mockResolvedValue({
      ...detail(9, "周会纪要"),
      transcript: "第一段",
      historyId: 42,
      kind: "import",
    });

    const note = await createNoteFromHistory(42);

    expect(api.createNoteFromHistory).toHaveBeenCalledWith(42);
    expect(note.id).toBe(9);
    // 存完直接切过去：用户的意图就是看这条笔记
    expect(getNotesSnapshot().selectedNoteId).toBe(9);
    expect(getNotesSnapshot().note?.historyId).toBe(42);
    // 列表也要刷，否则侧栏里看不到新笔记
    expect(api.listNotes).toHaveBeenCalledTimes(2);
  });

  it("surfaces the backend message instead of swallowing a failed save-as-note", async () => {
    await ensureNotesLoaded();
    api.createNoteFromHistory.mockRejectedValue(new Error("这条记录没有转写文本，无法存为笔记"));

    await expect(createNoteFromHistory(42)).rejects.toThrow("没有转写文本");
    expect(getNotesSnapshot().selectedNoteId).toBeNull();
  });
});
