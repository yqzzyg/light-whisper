import { beforeEach, describe, expect, it, vi } from "vitest";

const api = vi.hoisted(() => ({
  transcribeNoteRecording: vi.fn(),
  transcribeNoteRecordingSegment: vi.fn(),
}));
vi.mock("@/api/tauri", () => api);

const notes = vi.hoisted(() => ({
  refreshNotes: vi.fn(async () => {}),
  reloadOpenNote: vi.fn(async () => {}),
}));
vi.mock("@/hooks/notesStore", () => notes);

import {
  estimateTranscribeSeconds,
  getNoteTranscriptionSnapshot,
  resetNoteTranscriptionForTests,
  transcribeNote,
  transcribeSegment,
} from "@/hooks/noteTranscriptionStore";
import type { NoteDetail } from "@/types/notes";

function detail(overrides: Partial<NoteDetail> = {}): NoteDetail {
  return {
    id: 1,
    folderId: null,
    title: "周会",
    content: "",
    transcript: "大家好",
    summary: null,
    summaryModel: null,
    summaryGeneratedAt: null,
    summarySourceHash: null,
    kind: "meeting",
    durationSec: 600,
    historyId: 9,
    audioFile: "note-1-1.wav",
    createdAt: 1,
    updatedAt: 1_700_000_000_000,
    ...overrides,
  };
}

beforeEach(() => {
  resetNoteTranscriptionForTests();
  api.transcribeNoteRecording.mockReset();
  api.transcribeNoteRecordingSegment.mockReset();
  notes.refreshNotes.mockClear();
  notes.reloadOpenNote.mockClear();
});

describe("noteTranscriptionStore", () => {
  it("starts idle", () => {
    const snapshot = getNoteTranscriptionSnapshot();
    expect(snapshot.running).toBe(false);
    expect(snapshot.noteId).toBeNull();
    expect(snapshot.error).toBeNull();
  });

  it("marks the note as running and refreshes note plus list on success", async () => {
    let finish: (value: NoteDetail) => void = () => {};
    api.transcribeNoteRecording.mockImplementation(
      () => new Promise<NoteDetail>((done) => {
        finish = done;
      }),
    );

    const pending = transcribeNote(1);

    expect(getNoteTranscriptionSnapshot().running).toBe(true);
    expect(getNoteTranscriptionSnapshot().noteId).toBe(1);

    finish(detail());
    await expect(pending).resolves.toBeNull();

    expect(notes.reloadOpenNote).toHaveBeenCalledTimes(1);
    expect(notes.refreshNotes).toHaveBeenCalledTimes(1);
    expect(getNoteTranscriptionSnapshot().running).toBe(false);
    expect(getNoteTranscriptionSnapshot().noteId).toBeNull();
  });

  it("keeps the failure message and stops running so the button can retry", async () => {
    api.transcribeNoteRecording.mockRejectedValue("语音识别引擎尚未就绪");

    const error = await transcribeNote(7);

    expect(error).toBe("语音识别引擎尚未就绪");
    const snapshot = getNoteTranscriptionSnapshot();
    expect(snapshot.running).toBe(false);
    expect(snapshot.error).toEqual({ noteId: 7, message: "语音识别引擎尚未就绪" });
    // 失败不该刷新：库里没有任何变化。
    expect(notes.reloadOpenNote).not.toHaveBeenCalled();
  });

  it("ignores a second request while one is already running", async () => {
    api.transcribeNoteRecording.mockImplementation(() => new Promise<NoteDetail>(() => {}));

    void transcribeNote(1);
    await expect(transcribeNote(2)).resolves.toBeNull();

    expect(api.transcribeNoteRecording).toHaveBeenCalledTimes(1);
    expect(getNoteTranscriptionSnapshot().noteId).toBe(1);
  });

  it("tracks the active recording id for a single-segment re-transcribe", async () => {
    let finish: (value: NoteDetail) => void = () => {};
    api.transcribeNoteRecordingSegment.mockImplementation(
      () => new Promise<NoteDetail>((done) => {
        finish = done;
      }),
    );

    const pending = transcribeSegment(3, 42);

    const snapshot = getNoteTranscriptionSnapshot();
    expect(snapshot.running).toBe(true);
    expect(snapshot.noteId).toBe(3);
    expect(snapshot.recordingId).toBe(42);
    expect(api.transcribeNoteRecordingSegment).toHaveBeenCalledWith(42);
    // 单段转录只调段命令，不能顺带整篇命令。
    expect(api.transcribeNoteRecording).not.toHaveBeenCalled();

    finish(detail({ id: 3 }));
    await expect(pending).resolves.toBeNull();
    expect(notes.reloadOpenNote).toHaveBeenCalledTimes(1);
    expect(notes.refreshNotes).toHaveBeenCalledTimes(1);
    expect(getNoteTranscriptionSnapshot().recordingId).toBeNull();
  });

  it("keeps the segment failure and clears the active segment", async () => {
    api.transcribeNoteRecordingSegment.mockRejectedValue("引擎未就绪");

    const error = await transcribeSegment(3, 42);

    expect(error).toBe("引擎未就绪");
    expect(getNoteTranscriptionSnapshot().error).toEqual({
      noteId: 3,
      message: "引擎未就绪",
    });
    expect(getNoteTranscriptionSnapshot().recordingId).toBeNull();
    expect(notes.reloadOpenNote).not.toHaveBeenCalled();
  });

  it("estimates a wall-clock time from the recorded duration", () => {
    // 19× 实时：10 分钟录音约 32 秒。
    expect(estimateTranscribeSeconds(600)).toBe(32);
    // 极短录音给下限，避免显示「0 秒」像卡住。
    expect(estimateTranscribeSeconds(6)).toBe(5);
    expect(estimateTranscribeSeconds(null)).toBeNull();
    expect(estimateTranscribeSeconds(0)).toBeNull();
    expect(estimateTranscribeSeconds(Number.NaN)).toBeNull();
  });
});
