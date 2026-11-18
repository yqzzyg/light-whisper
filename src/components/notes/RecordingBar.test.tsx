import { fireEvent, render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

const store = vi.hoisted(() => ({
  useNoteRecording: vi.fn(),
  stopRecording: vi.fn(),
  cancelRecording: vi.fn(),
}));
vi.mock("@/hooks/noteRecordingStore", () => store);

const toast = vi.hoisted(() => ({ success: vi.fn(), error: vi.fn() }));
vi.mock("sonner", () => ({ toast }));

vi.mock("react-i18next", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));

import RecordingBar from "@/components/notes/RecordingBar";
import type { NoteRecordingSnapshot } from "@/types/notes";

function snapshot(overrides: Partial<NoteRecordingSnapshot> = {}): NoteRecordingSnapshot {
  return {
    active: true,
    sessionId: 1,
    noteId: 7,
    elapsedSec: 0,
    level: 0,
    audioFile: null,
    durationSec: null,
    error: null,
    ...overrides,
  };
}

beforeEach(() => {
  store.useNoteRecording.mockReset();
  store.stopRecording.mockReset().mockResolvedValue(null);
  store.cancelRecording.mockReset().mockResolvedValue(null);
  toast.error.mockReset();
});

describe("RecordingBar", () => {
  it("renders nothing while idle so the editor keeps its full height", () => {
    store.useNoteRecording.mockReturnValue(snapshot({ active: false }));
    const { container } = render(<RecordingBar />);
    expect(container.firstChild).toBeNull();
  });

  it("shows the elapsed time coming from the backend ticks", () => {
    store.useNoteRecording.mockReturnValue(snapshot({ elapsedSec: 65 }));
    render(<RecordingBar />);
    expect(screen.getByText("01:05")).toBeTruthy();
    expect(screen.getByText("notes.recording")).toBeTruthy();
  });

  it("scales the level bar with the reported peak and clamps out-of-range values", () => {
    store.useNoteRecording.mockReturnValue(snapshot({ level: 1.8 }));
    const { container } = render(<RecordingBar />);
    const level = container.querySelector<HTMLElement>(".notes-recording-level");
    expect(level?.style.width).toBe("100%");
  });

  it("stops the recording and notifies the page", async () => {
    const onStopped = vi.fn();
    store.useNoteRecording.mockReturnValue(snapshot({ elapsedSec: 12 }));
    render(<RecordingBar onStopped={onStopped} />);

    fireEvent.click(screen.getByText("notes.recordStop"));

    expect(store.stopRecording).toHaveBeenCalledTimes(1);
    await vi.waitFor(() => expect(onStopped).toHaveBeenCalledTimes(1));
  });

  it("reports a stop failure but still asks the page to re-read the note", async () => {
    const onStopped = vi.fn();
    store.stopRecording.mockResolvedValue("磁盘已满");
    store.useNoteRecording.mockReturnValue(snapshot());
    render(<RecordingBar onStopped={onStopped} />);

    fireEvent.click(screen.getByText("notes.recordStop"));

    await vi.waitFor(() => expect(toast.error).toHaveBeenCalledWith("notes.recordFailed"));
    expect(onStopped).toHaveBeenCalledTimes(1);
  });

  it("asks for confirmation before discarding a recording", () => {
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(false);
    store.useNoteRecording.mockReturnValue(snapshot());
    render(<RecordingBar />);

    fireEvent.click(screen.getByText("notes.recordCancel"));
    expect(confirm).toHaveBeenCalledTimes(1);
    expect(store.cancelRecording).not.toHaveBeenCalled();

    confirm.mockReturnValue(true);
    fireEvent.click(screen.getByText("notes.recordCancel"));
    expect(store.cancelRecording).toHaveBeenCalledTimes(1);
    confirm.mockRestore();
  });
});
