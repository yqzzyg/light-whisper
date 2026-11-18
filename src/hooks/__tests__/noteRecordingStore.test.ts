import { beforeEach, describe, expect, it, vi } from "vitest";

type Handler = (event: { payload: unknown }) => void;

const harness = vi.hoisted(() => ({
  handler: null as Handler | null,
  unlisten: vi.fn(),
}));

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async (_name: string, handler: Handler) => {
    harness.handler = handler;
    return harness.unlisten;
  }),
}));

const api = vi.hoisted(() => ({
  startNoteRecording: vi.fn(),
  stopNoteRecording: vi.fn(),
  cancelNoteRecording: vi.fn(),
  getNoteRecordingState: vi.fn(),
}));
vi.mock("@/api/tauri", () => api);

const notes = vi.hoisted(() => ({
  refreshNotes: vi.fn(async () => {}),
  reloadOpenNote: vi.fn(async () => {}),
}));
vi.mock("@/hooks/notesStore", () => notes);

import {
  cancelRecording,
  ensureNoteRecordingReady,
  getNoteRecordingSnapshot,
  resetNoteRecordingForTests,
  startRecording,
  stopRecording,
} from "@/hooks/noteRecordingStore";
import type { NoteRecordingSnapshot } from "@/types/notes";

function snapshot(overrides: Partial<NoteRecordingSnapshot> = {}): NoteRecordingSnapshot {
  return {
    active: true,
    sessionId: 5,
    noteId: 9,
    elapsedSec: 3,
    level: 0.4,
    audioFile: null,
    durationSec: null,
    error: null,
    ...overrides,
  };
}

beforeEach(() => {
  resetNoteRecordingForTests();
  harness.handler = null;
  harness.unlisten.mockReset();
  api.startNoteRecording.mockReset();
  api.stopNoteRecording.mockReset();
  api.cancelNoteRecording.mockReset();
  api.getNoteRecordingState.mockReset().mockResolvedValue(snapshot({ active: false }));
  notes.refreshNotes.mockClear();
  notes.reloadOpenNote.mockClear();
});

describe("noteRecordingStore", () => {
  it("starts idle and syncs once from the backend on first mount", async () => {
    expect(getNoteRecordingSnapshot().active).toBe(false);

    await ensureNoteRecordingReady();

    expect(api.getNoteRecordingState).toHaveBeenCalledTimes(1);
    expect(getNoteRecordingSnapshot().active).toBe(false);
  });

  it("subscribes only once even when several pages ask", async () => {
    await ensureNoteRecordingReady();
    await ensureNoteRecordingReady();

    expect(harness.handler).not.toBeNull();
    expect(api.getNoteRecordingState).toHaveBeenCalledTimes(2);
  });

  it("tracks backend ticks while recording", async () => {
    await ensureNoteRecordingReady();

    harness.handler?.({ payload: snapshot({ elapsedSec: 42, level: 0.9 }) });

    expect(getNoteRecordingSnapshot().elapsedSec).toBe(42);
    expect(notes.refreshNotes).not.toHaveBeenCalled();
  });

  it("refreshes the note and the list after the recording stops", async () => {
    await ensureNoteRecordingReady();

    harness.handler?.({
      payload: snapshot({ active: false, audioFile: "note-9-1.wav", durationSec: 12.5 }),
    });

    expect(notes.reloadOpenNote).toHaveBeenCalledTimes(1);
    expect(notes.refreshNotes).toHaveBeenCalledTimes(1);
    expect(getNoteRecordingSnapshot().audioFile).toBe("note-9-1.wav");
  });

  it("returns the backend message when starting is refused", async () => {
    api.startNoteRecording.mockRejectedValue("听写正在进行，请先结束听写再开始会议录音");

    const error = await startRecording(1);

    expect(error).toBe("听写正在进行，请先结束听写再开始会议录音");
    expect(getNoteRecordingSnapshot().active).toBe(false);
  });

  it("surfaces a write error reported by stop without losing the file name", async () => {
    api.stopNoteRecording.mockResolvedValue(
      snapshot({
        active: false,
        audioFile: "note-9-2.wav",
        durationSec: 30,
        error: "写入录音数据失败（磁盘可能已满）",
      }),
    );

    const error = await stopRecording();

    expect(error).toBe("写入录音数据失败（磁盘可能已满）");
    expect(getNoteRecordingSnapshot().audioFile).toBe("note-9-2.wav");
    expect(notes.reloadOpenNote).toHaveBeenCalledTimes(1);
  });

  it("drops back to idle after discarding", async () => {
    api.cancelNoteRecording.mockResolvedValue(undefined);
    await ensureNoteRecordingReady();
    harness.handler?.({ payload: snapshot() });
    expect(getNoteRecordingSnapshot().active).toBe(true);

    await cancelRecording();

    expect(getNoteRecordingSnapshot().active).toBe(false);
    expect(getNoteRecordingSnapshot().noteId).toBeNull();
  });
});
