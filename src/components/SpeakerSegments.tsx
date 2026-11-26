import { useTranslation } from "react-i18next";
import { Users } from "lucide-react";

import { formatDuration } from "@/lib/duration";
import type { HistorySpeakerSegment } from "@/types";

/**
 * 按说话人渲染一段转写。
 *
 * 与 `notes/SpeakerTranscript` 的区别只有一个：这里的分段属于**一次转写**
 * （一条历史、一段录音），时间轴只有一条，所以不需要按录音分组。笔记那边
 * 一篇笔记可以有多段录音、各自时间轴都从 0 开始，混在一起会重叠，才必须分组。
 *
 * 说话人编号是本次转写内的聚类编号，跨历史没有可比性，因此只按 `speakerId`
 * 轮转配色（见 CSS 的 `[data-speaker]`），不跨记录合并同一个人。
 */
export default function SpeakerSegments({
  segments,
}: {
  segments: HistorySpeakerSegment[];
}) {
  const { t } = useTranslation();

  if (segments.length === 0) return null;

  const speakerCount = new Set(segments.map((segment) => segment.speakerId)).size;

  return (
    <div className="notes-speakers">
      <div className="notes-speakers-head">
        <Users size={12} aria-hidden="true" />
        <span>{t("notes.speakerCount", { count: speakerCount })}</span>
        <span className="notes-speakers-hint">{t("result.speakerPlaybackHint")}</span>
      </div>
      <div className="notes-panel-scroll">
        <ol className="notes-speaker-turns">
          {segments.map((segment, index) => (
            <li
              // 同一毫秒内理论上不会有两段，但 react 的 key 要保证唯一，
              // 所以把下标带上；分段列表本身是只读的，不用考虑移动动画。
              key={`${segment.startMs}-${index}`}
              className="notes-speaker-turn"
              data-speaker={segment.speakerId % 6}
            >
              <div className="notes-speaker-meta">
                <span className="notes-speaker-name">
                  {t("notes.speaker", { index: segment.speakerId + 1 })}
                </span>
                <span className="notes-speaker-time">
                  {formatDuration(segment.startMs / 1000)}
                </span>
                {segment.overlap && (
                  <span className="notes-speaker-overlap" title={t("notes.speakerOverlap")}>
                    ⁓
                  </span>
                )}
              </div>
              <p className="notes-speaker-text">{segment.text}</p>
            </li>
          ))}
        </ol>
      </div>
    </div>
  );
}
