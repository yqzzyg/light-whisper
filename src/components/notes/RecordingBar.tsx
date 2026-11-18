import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { Mic, Square, Trash2 } from "lucide-react";

import { cancelRecording, stopRecording, useNoteRecording } from "@/hooks/noteRecordingStore";
import { formatDuration } from "@/lib/duration";

/**
 * 录音条：只在录音进行中渲染。
 *
 * 计时与电平都来自后端事件（500ms 一次），前端不再自己跑计时器——这样切页、
 * 最小化、甚至 WebView 被节流时，显示的仍是真实录音进度。
 */
export default function RecordingBar({
  onStopped,
}: {
  /** 停止成功后的回调（页面用它切到转录 tab 或刷新视图）。 */
  onStopped?: () => void;
}) {
  const { t } = useTranslation();
  const recording = useNoteRecording();

  if (!recording.active) return null;

  const stop = async () => {
    const error = await stopRecording();
    if (error) toast.error(t("notes.recordFailed", { message: error }));
    // 即使停止失败也要回调：音频已落盘/或写入出错，页面都应该重读一次笔记。
    onStopped?.();
  };

  const cancel = async () => {
    if (!window.confirm(t("notes.recordCancelConfirm"))) return;
    const error = await cancelRecording();
    if (error) toast.error(t("notes.recordFailed", { message: error }));
  };

  return (
    <div className="notes-recording-bar" role="status" aria-live="polite">
      <span className="notes-recording-dot" aria-hidden="true" />
      <Mic size={13} aria-hidden="true" />
      <span className="notes-recording-label">{t("notes.recording")}</span>
      <span className="notes-recording-time">{formatDuration(recording.elapsedSec)}</span>
      <span className="notes-recording-level-track" aria-hidden="true">
        <span
          className="notes-recording-level"
          style={{ width: `${Math.round(Math.min(1, Math.max(0, recording.level)) * 100)}%` }}
        />
      </span>
      <span className="notes-recording-spacer" />
      <button type="button" className="notes-recording-btn" onClick={() => void stop()}>
        <Square size={11} />
        {t("notes.recordStop")}
      </button>
      <button
        type="button"
        className="notes-recording-btn notes-recording-btn-quiet"
        onClick={() => void cancel()}
      >
        <Trash2 size={11} />
        {t("notes.recordCancel")}
      </button>
    </div>
  );
}
