import { render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

vi.mock("react-i18next", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));

import SpeakerTranscript from "@/components/notes/SpeakerTranscript";
import type { NoteRecording, NoteSpeakerSegment } from "@/types/notes";

function recording(id: number): NoteRecording {
  return {
    id,
    noteId: 1,
    audioFile: `${id}.wav`,
    durationSec: 60,
    transcript: null,
    historyId: null,
    sortOrder: id,
    createdAt: id,
  };
}

function turn(
  recordingId: number,
  speakerId: number,
  startMs: number,
  text: string,
  overlap = false,
): NoteSpeakerSegment {
  return { recordingId, speakerId, startMs, endMs: startMs + 1_000, text, overlap };
}

describe("SpeakerTranscript", () => {
  it("renders nothing when there are no segments", () => {
    // 空分段是「这次没分离」的正常状态，界面必须退回整段正文而不是留一块空白。
    const { container } = render(
      <SpeakerTranscript segments={[]} recordings={[recording(1)]} />,
    );
    expect(container).toBeEmptyDOMElement();
  });

  it("keeps one group when every turn belongs to the same recording", () => {
    render(
      <SpeakerTranscript
        segments={[turn(1, 0, 0, "甲说"), turn(1, 1, 2_000, "乙说")]}
        recordings={[recording(1)]}
      />,
    );

    expect(screen.getByText("甲说")).toBeInTheDocument();
    expect(screen.getByText("乙说")).toBeInTheDocument();
    // 只有一段录音时不该出现「第 n 段」的组标题，那是纯噪音。
    expect(screen.queryByText(/notes.recording/)).toBeNull();
  });

  it("starts a new group when the recording changes", () => {
    // 每段录音的说话人编号是独立的（时间轴各自从 0 开始），必须分组显示。
    render(
      <SpeakerTranscript
        segments={[turn(1, 0, 0, "第一段"), turn(2, 0, 0, "第二段")]}
        recordings={[recording(1), recording(2)]}
      />,
    );

    expect(screen.getByText("第一段")).toBeInTheDocument();
    expect(screen.getByText("第二段")).toBeInTheDocument();
    expect(screen.getAllByText(/notes.recording/)).toHaveLength(2);
  });

  it("marks overlapping turns so the reader knows two people talked", () => {
    render(
      <SpeakerTranscript
        segments={[turn(1, 0, 0, "插话", true)]}
        recordings={[recording(1)]}
      />,
    );
    expect(screen.getByTitle("notes.speakerOverlap")).toBeInTheDocument();
  });

  it("cycles the colour bucket instead of emitting unbounded indices", () => {
    // data-speaker 只用 0–5，CSS 只定义了这 6 组颜色；说话人再多也只能轮转。
    render(
      <SpeakerTranscript
        segments={[turn(1, 7, 0, "第七个人")]}
        recordings={[recording(1)]}
      />,
    );
    const item = screen.getByText("第七个人").closest("li");
    expect(item).toHaveAttribute("data-speaker", "1");
  });
});
