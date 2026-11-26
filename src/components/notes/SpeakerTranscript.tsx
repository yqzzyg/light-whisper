import { useTranslation } from "react-i18next";
import { Users } from "lucide-react";

import { formatDuration } from "@/lib/duration";
import type { NoteRecording, NoteSpeakerSegment } from "@/types/notes";

/**
 * 按说话人渲染转录。
 *
 * 分段的说话人编号是**每段录音独立**的（每段的时间轴都从 0 开始、每段单独跑分离），
 * 所以必须按 `recordingId` 分组显示，把多段录音的片段混成一个列表会出现两个
 * 「说话人 1」且时间倒流。只按录音分组、不跨录音合并说话人：跨段合并需要说话人
 * 嵌入聚类，属于后续版本，当前宁可显示得保守也不要把两个人认成一个。
 */
export default function SpeakerTranscript({
  segments,
  recordings,
}: {
  segments: NoteSpeakerSegment[];
  /** 只为拿到「第几段录音」的序号；分段本身按 recordingId 找归属。 */
  recordings: NoteRecording[];
}) {
  const { t } = useTranslation();

  if (segments.length === 0) return null;

  const groups: { recordingId: number; turns: NoteSpeakerSegment[] }[] = [];
  for (const segment of segments) {
    const last = groups[groups.length - 1];
    if (last && last.recordingId === segment.recordingId) {
      last.turns.push(segment);
    } else {
      groups.push({ recordingId: segment.recordingId, turns: [segment] });
    }
  }

  // 说话人数按录音内去重后取最大值：不跨录音算，避免把两个人算成一个。
  const speakerCount = Math.max(
    ...groups.map((group) => new Set(group.turns.map((turn) => turn.speakerId)).size),
  );

  return (
    <div className="notes-speakers">
      <div className="notes-speakers-head">
        <Users size={12} />
        <span>{t("notes.speakerCount", { count: speakerCount })}</span>
        <span className="notes-speakers-hint">{t("notes.speakerSegmentHint")}</span>
      </div>
      <div className="notes-panel-scroll">
        {groups.map((group) => {
          const index = recordings.findIndex((item) => item.id === group.recordingId);
          return (
            <section key={group.recordingId} className="notes-speaker-group">
              {/* 只有一段录音时不必标「第 1 段」，那是纯噪音。 */}
              {groups.length > 1 && (
                <div className="notes-speaker-group-head">
                  {t("notes.recording")} {index >= 0 ? index + 1 : 1}
                </div>
              )}
              <ol className="notes-speaker-turns">
                {group.turns.map((turn, position) => (
                  <li
                    key={`${turn.startMs}-${position}`}
                    className="notes-speaker-turn"
                    // 只用有限几种颜色（见 CSS 的 [data-speaker]），否则说话人一多就撞色。
                    data-speaker={turn.speakerId % 6}
                  >
                    <div className="notes-speaker-meta">
                      <span className="notes-speaker-name">
                        {t("notes.speaker", { index: turn.speakerId + 1 })}
                      </span>
                      <span className="notes-speaker-time">
                        {formatDuration(turn.startMs / 1000)}
                      </span>
                      {turn.overlap && (
                        <span
                          className="notes-speaker-overlap"
                          title={t("notes.speakerOverlap")}
                        >
                          ⁓
                        </span>
                      )}
                    </div>
                    <p className="notes-speaker-text">{turn.text}</p>
                  </li>
                ))}
              </ol>
            </section>
          );
        })}
      </div>
    </div>
  );
}
