import { useEffect, useState } from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import {
  AudioLines,
  Copy,
  Download,
  FileText,
  FolderOpen,
  LoaderCircle,
  Mic,
  Pause,
  Play,
  RefreshCw,
  Sparkles,
  Trash2,
  X,
} from "lucide-react";

import { copyToClipboard, getNoteRecordingAudioUrl, saveTextFile } from "@/api/tauri";
import MarkdownContent from "@/components/MarkdownContent";
import MarkdownEditor from "@/components/notes/MarkdownEditor";
import SpeakerTranscript from "@/components/notes/SpeakerTranscript";
import { formatDuration } from "@/lib/duration";
import type { NoteTranscriptionState } from "@/hooks/noteTranscriptionStore";
import {
  cancelSummary,
  ensureNoteSummarySubscribed,
  generateSummary,
  useNoteSummary,
} from "@/hooks/noteSummaryStore";
import type {
  NoteDetail,
  NoteFolder,
  NotePatch,
  NoteRecording,
  NoteRecordingSnapshot,
  NoteSpeakerSegment,
} from "@/types/notes";

type Tab = "content" | "transcript" | "summary";

const INVALID_FILE_CHARS = /[\\/:*?"<>|]/g;

/** 控制字符不走正则（eslint 禁止 no-control-regex），按码点过滤。 */
function isPrintable(char: string): boolean {
  const code = char.codePointAt(0) ?? 0;
  return code >= 0x20 && code !== 0x7f;
}

/** 导出文件名：去掉路径分隔符与 Windows 非法字符，空标题兜底成 note。 */
export function buildExportFileName(title: string, extension: string): string {
  const base = [...title]
    .filter(isPrintable)
    .join("")
    .replace(INVALID_FILE_CHARS, " ")
    .replace(/\s+/g, " ")
    .replace(/^[.\s]+|[.\s]+$/g, "")
    .slice(0, 60)
    .trim();
  return `${base || "note"}.${extension}`;
}

export interface ExportLabels {
  transcript: string;
  summary: string;
}

/**
 * 拼导出文档：只有标题、正文、转录、摘要四块，按当前界面语言加小节标题。
 * 放前端拼是为了让导出文档的语言跟界面一致，后端只负责落盘。
 */
export function composeNoteExport(
  note: Pick<NoteDetail, "title" | "content" | "transcript" | "summary">,
  format: "markdown" | "text",
  labels: ExportLabels,
): string {
  const markdown = format === "markdown";
  const parts: string[] = [];
  const title = note.title.trim();
  if (title) parts.push(markdown ? `# ${title}` : title);
  if (note.content.trim()) parts.push(note.content.trim());
  if (note.transcript?.trim()) {
    parts.push(markdown ? `## ${labels.transcript}` : labels.transcript);
    parts.push(note.transcript.trim());
  }
  if (note.summary?.trim()) {
    parts.push(markdown ? `## ${labels.summary}` : labels.summary);
    parts.push(note.summary.trim());
  }
  return `${parts.join("\n\n")}\n`;
}

export interface NoteEditorProps {
  note: NoteDetail | null;
  folders: NoteFolder[];
  loading: boolean;
  saving: boolean;
  untitled: string;
  /** 会议录音状态：用于禁用「录音」按钮与显示「已录制未转写」。 */
  recording: NoteRecordingSnapshot;
  /** 这条笔记的所有录音段。 */
  recordings: NoteRecording[];
  /** 说话人分段，跨录音段拍平；空数组表示这次没做分离，退回整段正文。 */
  speakerSegments: NoteSpeakerSegment[];
  /** 转写状态：跨页面保留，切回来仍在跑。 */
  transcription: NoteTranscriptionState;
  onStartRecording: () => void;
  onTranscribe: () => void;
  /** 整篇重新转写：忽略已有转录、覆盖重跑（换引擎时用，调用方负责二次确认）。 */
  onRetranscribe: () => void;
  onTranscribeSegment: (recordingId: number) => void;
  onRevealRecording: (id: number) => void;
  onDeleteRecording: (id: number) => void;
  onEdit: (patch: NotePatch) => void;
  onFlush: () => void;
  onMove: (folderId: number | null) => void;
  onDelete: () => void;
  onClose: () => void;
}

export default function NoteEditor({
  note,
  folders,
  loading,
  saving,
  untitled,
  recording,
  recordings,
  speakerSegments,
  transcription,
  onStartRecording,
  onTranscribe,
  onRetranscribe,
  onTranscribeSegment,
  onRevealRecording,
  onDeleteRecording,
  onEdit,
  onFlush,
  onMove,
  onDelete,
  onClose,
}: NoteEditorProps) {
  const { t, i18n } = useTranslation();
  const [tab, setTab] = useState<Tab>("content");
  /** 正在播放的段 id 与其 asset URL；同一时刻只展开一个播放器。 */
  const [playing, setPlaying] = useState<{ id: number; url: string } | null>(null);
  // 摘要进度跟着后端命令走（模块级 store），切走页面再回来仍能看到。
  const summaryJob = useNoteSummary();

  useEffect(() => {
    void ensureNoteSummarySubscribed();
  }, []);

  // 换一条笔记时回到正文 tab，并收起播放器：上一条的状态对这条没有意义。
  useEffect(() => {
    setTab("content");
    setPlaying(null);
  }, [note?.id]);

  /** 解析该段的本地路径并转成 asset URL，展开原生播放器自动开始播放。 */
  const playRecording = async (id: number) => {
    if (playing?.id === id) {
      setPlaying(null);
      return;
    }
    try {
      const path = await getNoteRecordingAudioUrl(id);
      setPlaying({ id, url: convertFileSrc(path) });
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      toast.error(t("notes.playAudioFailed", { message }));
    }
  };

  if (!note) {
    return (
      <section className="notes-editor">
        <div className="notes-empty notes-editor-empty">
          <FileText size={24} strokeWidth={1.5} />
          <p>{t("notes.selectHint")}</p>
        </div>
      </section>
    );
  }

  if (loading) {
    return (
      <section className="notes-editor">
        <div className="notes-empty notes-editor-empty">
          <LoaderCircle size={18} className="animate-spin" />
        </div>
      </section>
    );
  }

  const transcript = note.transcript ?? "";
  const summary = note.summary ?? "";
  // 导出的是一条笔记的全量（标题 + 正文 + 转录 + 摘要），任意一块有字即可；
  // 摘要不是导出的前置条件。
  const hasExportableContent = Boolean(
    note.title.trim() || note.content.trim() || transcript.trim() || summary.trim(),
  );
  // 转写状态是全局的（一次只跑一篇）：只有当前笔记在跑时才显示进度，
  // 失败提示也只挂回出错那篇，切到别的笔记不会看到无关的报错。
  const isTranscribing = transcription.running && transcription.noteId === note.id;
  const transcribeBusy = transcription.running && transcription.noteId !== note.id;
  const transcribeError =
    !transcription.running && transcription.error?.noteId === note.id
      ? transcription.error.message
      : null;
  const pendingRecordings = recordings.filter((item) => !item.transcript?.trim());
  const pendingDuration = pendingRecordings.reduce(
    (sum, item) => sum + (item.durationSec ?? 0),
    0,
  );
  // 是否还有段没转写：只有段表是录音的事实源（`notes.audio_file` 迁移后恒为空，
  // 不再参与判断）。从历史转存来的笔记没有段，也不会误判成待转写。
  const transcriptPending = pendingRecordings.length > 0;
  const updatedLabel = new Intl.DateTimeFormat(i18n.language, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  }).format(new Date(note.updatedAt));

  const summarizingThis = summaryJob.running && summaryJob.noteId === note.id;
  const summarizingElsewhere = summaryJob.running && summaryJob.noteId !== note.id;
  const summaryError =
    !summaryJob.running && summaryJob.error?.noteId === note.id
      ? summaryJob.error.message
      : null;
  // 没有转录就没有素材：后端也会拦，但按钮要先灰掉，别让用户白点一次。
  const canGenerateSummary = Boolean(transcript.trim()) && !summaryJob.running;
  const summaryProgressLabel =
    summaryJob.phase === "part" && summaryJob.total > 1
      ? t("notes.summaryPart", { part: summaryJob.part, total: summaryJob.total })
      : summaryJob.phase === "merging" || summaryJob.phase === "streaming"
        ? t("notes.summaryMerging")
        : t("notes.summaryPreparing");

  const runSummary = async () => {
    const message = await generateSummary(note.id);
    if (message) toast.error(t("notes.summaryFailed", { message }));
  };

  const copySummary = async () => {
    try {
      await copyToClipboard(summary);
      toast.success(t("notes.copied"));
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      toast.error(t("notes.copyFailed", { message }));
    }
  };

  const labels: ExportLabels = {
    transcript: t("notes.transcript"),
    summary: t("notes.summary"),
  };

  const exportNote = async (format: "markdown" | "text") => {
    const extension = format === "markdown" ? "md" : "txt";
    try {
      const path = await saveTextFile(
        buildExportFileName(note.title, extension),
        composeNoteExport(note, format, labels),
      );
      if (path) toast.success(t("notes.exported"));
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      toast.error(t("notes.exportFailed", { message }));
    }
  };

  const copyTranscript = async () => {
    try {
      await copyToClipboard(transcript);
      toast.success(t("notes.copied"));
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      toast.error(t("notes.copyFailed", { message }));
    }
  };

  return (
    <section className="notes-editor">
      <div className="notes-editor-head">
        <input
          className="notes-title-input"
          value={note.title}
          placeholder={untitled}
          onChange={(event) => onEdit({ title: event.target.value })}
          onBlur={onFlush}
        />
        <div className="notes-editor-meta">
          <select
            className="notes-folder-select"
            value={note.folderId ?? ""}
            onChange={(event) => {
              const value = event.target.value;
              onMove(value === "" ? null : Number(value));
            }}
          >
            <option value="">{t("notes.unfiled")}</option>
            {folders.map((folder) => (
              <option key={folder.id} value={folder.id}>
                {folder.name}
              </option>
            ))}
          </select>
          <span className="notes-editor-time">{updatedLabel}</span>
          <span className="notes-save-state" data-saving={saving}>
            {saving ? t("notes.saving") : t("notes.saved")}
          </span>
          <span className="notes-editor-spacer" />
          <button
            type="button"
            className="icon-btn plain"
            aria-label={t("notes.recordStart")}
            title={
              recording.active ? t("notes.recordBusy") : t("notes.recordStart")
            }
            disabled={recording.active}
            onClick={onStartRecording}
          >
            <Mic size={14} />
          </button>
          <button
            type="button"
            className="icon-btn plain"
            aria-label={t("common.delete")}
            title={t("common.delete")}
            onClick={onDelete}
          >
            <Trash2 size={14} />
          </button>
          <button
            type="button"
            className="icon-btn plain"
            aria-label={t("common.close")}
            title={t("common.close")}
            onClick={onClose}
          >
            <X size={14} />
          </button>
        </div>
      </div>

      <div className="notes-tabs" role="tablist" aria-label={t("notes.title")}>
        <button
          type="button"
          role="tab"
          className="notes-tab"
          aria-selected={tab === "content"}
          data-active={tab === "content"}
          onClick={() => setTab("content")}
        >
          {t("notes.myNotes")}
        </button>
        <button
          type="button"
          role="tab"
          className="notes-tab"
          aria-selected={tab === "transcript"}
          data-active={tab === "transcript"}
          onClick={() => setTab("transcript")}
        >
          {t("notes.transcript")}
          {/* 有录音没转录时点成告警色：用户停在「我的笔记」也能看到还差一步。 */}
          {(transcript.trim() || transcriptPending) && (
            <span
              className="notes-tab-dot"
              data-pending={!transcript.trim()}
              aria-hidden="true"
            />
          )}
        </button>
        <button
          type="button"
          role="tab"
          className="notes-tab"
          aria-selected={tab === "summary"}
          data-active={tab === "summary"}
          onClick={() => setTab("summary")}
        >
          {t("notes.summary")}
          {summary.trim() && <span className="notes-tab-dot" aria-hidden="true" />}
        </button>
      </div>

      {tab === "content" && (
        <MarkdownEditor
          // 换笔记直接重建编辑器：否则撤销栈会跨笔记生效，Ctrl+Z 能把上一篇的
          // 内容盖回来，而自动保存会把它写进这一篇。
          key={note.id}
          value={note.content}
          placeholder={t("notes.contentPlaceholder")}
          onChange={(content) => onEdit({ content })}
          onBlur={onFlush}
          onExport={() => void exportNote("markdown")}
          exportDisabled={!hasExportableContent}
        />
      )}

      {tab === "transcript" && (
        <div className="notes-panel">
          {/* 从历史/上传转来的笔记：音频留在历史记录里，这里要说清去哪听，
              否则用户会以为录音丢了。 */}
          {note.historyId && recordings.length === 0 && (
            <p className="notes-audio-hint">{t("notes.audioInHistory")}</p>
          )}
          {/* 先录后转：录音已经在本地但还没转录，说清「有什么、缺什么」，
              并给出唯一的下一步——把待转写的段转成文字。 */}
          {transcriptPending && (
            <div className="notes-transcribe">
              <p className="notes-audio-hint">
                {t("notes.recordedNotTranscribed", {
                  duration: formatDuration(pendingDuration > 0 ? pendingDuration : note.durationSec),
                })}
              </p>
              {isTranscribing ? (
                <p className="notes-transcribe-status" role="status">
                  <LoaderCircle size={12} className="animate-spin" />
                  <span>
                    {transcription.chunk
                      ? t("notes.transcribingChunk", {
                          chunk: transcription.chunk.done,
                          elapsed: formatDuration(transcription.elapsedSec),
                        })
                      : t("notes.transcribing", {
                          elapsed: formatDuration(transcription.elapsedSec),
                        })}
                  </span>
                  <span className="notes-transcribe-hint">{t("notes.transcribeCanLeave")}</span>
                </p>
              ) : (
                <div className="notes-transcribe-actions">
                  <button
                    type="button"
                    className="notes-transcribe-btn"
                    disabled={transcribeBusy}
                    title={transcribeBusy ? t("notes.transcribeBusy") : t("notes.transcribe")}
                    onClick={onTranscribe}
                  >
                    <AudioLines size={12} />
                    {t("notes.transcribe")}
                  </button>
                  <span className="notes-transcribe-hint">
                    {t("notes.transcribeCanLeave")}
                  </span>
                </div>
              )}
              {transcribeError && (
                <p className="notes-transcribe-error" role="alert">
                  {t("notes.transcribeFailed", { message: transcribeError })}
                </p>
              )}
            </div>
          )}
          <div className="notes-toolbar">
            <span>{t("notes.charCount", { count: transcript.trim().length })}</span>
            <span className="notes-toolbar-spacer" />
            {/* 整篇重转：换了引擎、或整段识别都不理想时用。它会覆盖现有转录，
                所以调用方必须先问一次；没有录音段的笔记不出现这个入口。 */}
            {transcript.trim() && recordings.length > 0 && (
              <button
                type="button"
                className="notes-toolbar-btn"
                disabled={transcription.running}
                title={t("notes.retranscribeAllHint")}
                onClick={onRetranscribe}
              >
                <RefreshCw size={12} />
                {t("notes.retranscribeAll")}
              </button>
            )}
            <button
              type="button"
              className="notes-toolbar-btn"
              disabled={!transcript.trim()}
              onClick={() => void copyTranscript()}
            >
              <Copy size={12} />
              {t("notes.copy")}
            </button>
            <button
              type="button"
              className="notes-toolbar-btn"
              disabled={!transcript.trim()}
              onClick={() => void exportNote("text")}
            >
              <Download size={12} />
              {t("notes.exportTxt")}
            </button>
          </div>
          {recordings.length > 0 && (
            <div className="notes-recordings">
              <div className="notes-recordings-head">{t("notes.recordings")}</div>
              {recordings.map((item, index) => {
                const done = Boolean(item.transcript?.trim());
                const thisTranscribing =
                  isTranscribing && transcription.recordingId === item.id;
                const canAct = !transcription.running;
                const isPlaying = playing?.id === item.id;
                return (
                  <div key={item.id}>
                    <div className="notes-recording-row">
                      <span className="notes-recording-index">{index + 1}</span>
                      <span className="notes-recording-duration">
                        {formatDuration(item.durationSec)}
                      </span>
                      <span
                        className="notes-recording-status"
                        data-pending={!done}
                      >
                        {thisTranscribing
                          ? t("notes.segmentTranscribing")
                          : t(done ? "notes.hasTranscript" : "notes.notTranscribed")}
                      </span>
                      <span className="notes-recording-spacer" />
                      {/* 应用内播放：点一下展开原生播放器，再点收起。 */}
                      <button
                        type="button"
                        className="notes-toolbar-btn"
                        data-active={isPlaying}
                        title={t(isPlaying ? "notes.stopPlayAudio" : "notes.playAudio")}
                        aria-label={t(isPlaying ? "notes.stopPlayAudio" : "notes.playAudio")}
                        onClick={() => void playRecording(item.id)}
                      >
                        {isPlaying ? <Pause size={12} /> : <Play size={12} />}
                      </button>
                      {/* 单段重转：已转写的段也能重来，挑出识别不佳的那一段。 */}
                      <button
                        type="button"
                        className="notes-toolbar-btn"
                        disabled={!canAct}
                        title={t(done ? "notes.retranscribeSegment" : "notes.transcribeSegment")}
                        aria-label={t(done ? "notes.retranscribeSegment" : "notes.transcribeSegment")}
                        onClick={() => onTranscribeSegment(item.id)}
                      >
                        {thisTranscribing ? (
                          <LoaderCircle size={12} className="animate-spin" />
                        ) : (
                          <AudioLines size={12} />
                        )}
                      </button>
                      <button
                        type="button"
                        className="notes-toolbar-btn"
                        title={t("notes.revealAudio")}
                        aria-label={t("notes.revealAudio")}
                        onClick={() => onRevealRecording(item.id)}
                      >
                        <FolderOpen size={12} />
                      </button>
                      <button
                        type="button"
                        className="notes-toolbar-btn"
                        disabled={!canAct}
                        title={t("notes.deleteRecording")}
                        aria-label={t("notes.deleteRecording")}
                        onClick={() => onDeleteRecording(item.id)}
                      >
                        <Trash2 size={12} />
                      </button>
                    </div>
                    {isPlaying && (
                      <div className="notes-recording-player">
                        {/* 会议录音是纯音频，没有 WebVTT 字幕轨；文字转录在同一页的
                            转录区展示，这里不需要（也无法）挂 <track>。 */}
                        {/* eslint-disable-next-line jsx-a11y/media-has-caption */}
                        <audio
                          controls
                          autoPlay
                          src={playing.url}
                          onEnded={() => setPlaying(null)}
                        />
                      </div>
                    )}
                  </div>
                );
              })}
            </div>
          )}
          {transcript.trim() ? (
            speakerSegments.length > 0 ? (
              <SpeakerTranscript segments={speakerSegments} recordings={recordings} />
            ) : (
              <div className="notes-panel-scroll">
                <p className="notes-transcript-text">{transcript}</p>
              </div>
            )
          ) : (
            <div className="notes-panel-empty">
              <FileText size={22} strokeWidth={1.5} />
              <p>{t("notes.transcriptEmpty")}</p>
              <p className="notes-panel-hint">{t("notes.transcriptHint")}</p>
            </div>
          )}
        </div>
      )}

      {tab === "summary" && (
        <div className="notes-panel">
          <div className="notes-toolbar">
            <span className="notes-toolbar-spacer" />
            {summary.trim() && !summarizingThis && (
              <button
                type="button"
                className="notes-toolbar-btn"
                onClick={() => void copySummary()}
              >
                <Copy size={12} />
                {t("notes.copy")}
              </button>
            )}
            {summarizingThis ? (
              <button
                type="button"
                className="notes-toolbar-btn"
                onClick={() => void cancelSummary()}
              >
                <X size={12} />
                {t("notes.summaryCancel")}
              </button>
            ) : (
              <button
                type="button"
                className="notes-toolbar-btn"
                disabled={!canGenerateSummary}
                title={transcript.trim() ? undefined : t("notes.summaryNeedTranscript")}
                onClick={() => void runSummary()}
              >
                <Sparkles size={12} />
                {summary.trim() ? t("notes.summaryRegenerate") : t("notes.summaryGenerate")}
              </button>
            )}
          </div>

          {note.summaryStale && summary.trim() && !summarizingThis && (
            <p className="notes-summary-stale">{t("notes.summaryStale")}</p>
          )}
          {summarizingElsewhere && (
            <p className="notes-audio-hint">{t("notes.summaryBusy")}</p>
          )}

          {summarizingThis ? (
            <div className="notes-summary-progress" role="status">
              <p className="notes-transcribe-status">
                <LoaderCircle size={12} className="animate-spin" />
                <span>{summaryProgressLabel}</span>
              </p>
              {summaryJob.preview && (
                <div className="notes-summary-preview">
                  <MarkdownContent content={summaryJob.preview} />
                </div>
              )}
            </div>
          ) : summary.trim() ? (
            <div className="notes-panel-scroll">
              <MarkdownContent content={summary} />
            </div>
          ) : (
            <div className="notes-panel-empty">
              <Sparkles size={22} strokeWidth={1.5} />
              <p>{t("notes.summaryEmpty")}</p>
              <p className="notes-panel-hint">
                {transcript.trim() ? t("notes.summaryHint") : t("notes.summaryNeedTranscript")}
              </p>
            </div>
          )}

          {summaryError && (
            <p className="notes-transcribe-error" role="alert">
              {t("notes.summaryFailed", { message: summaryError })}
            </p>
          )}
        </div>
      )}
    </section>
  );
}
