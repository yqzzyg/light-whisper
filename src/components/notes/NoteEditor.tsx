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
  Sparkles,
  Trash2,
  X,
} from "lucide-react";

import { copyToClipboard, getNoteRecordingAudioUrl, saveTextFile } from "@/api/tauri";
import MarkdownContent from "@/components/MarkdownContent";
import MarkdownEditor from "@/components/notes/MarkdownEditor";
import { formatDuration } from "@/lib/duration";
import {
  estimateTranscribeSeconds,
  type NoteTranscriptionState,
} from "@/hooks/noteTranscriptionStore";
import type {
  NoteDetail,
  NoteFolder,
  NotePatch,
  NoteRecording,
  NoteRecordingSnapshot,
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
  /** 转写状态：跨页面保留，切回来仍在跑。 */
  transcription: NoteTranscriptionState;
  onStartRecording: () => void;
  onTranscribe: () => void;
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
  transcription,
  onStartRecording,
  onTranscribe,
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
  // 导出的是一条笔记的全量（标题 + 正文 + 转录 + 摘要），所以只要有任意一块
  // 有字就允许导出；以前只在「摘要」tab 提供入口，没写摘要的笔记就导不出来。
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
  const estimate = estimateTranscribeSeconds(
    pendingDuration > 0 ? pendingDuration : note.durationSec,
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
                    {t("notes.transcribing", {
                      elapsed: formatDuration(transcription.elapsedSec),
                    })}
                  </span>
                  {estimate !== null && (
                    <span className="notes-transcribe-estimate">
                      {t("notes.transcribeEstimate", { estimate: formatDuration(estimate) })}
                    </span>
                  )}
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
                  {estimate !== null && (
                    <span className="notes-transcribe-hint">
                      {t("notes.transcribeHint", { estimate: formatDuration(estimate) })}
                    </span>
                  )}
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
            <div className="notes-panel-scroll">
              <p className="notes-transcript-text">{transcript}</p>
            </div>
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
          {summary.trim() ? (
            <div className="notes-panel-scroll">
              <MarkdownContent content={summary} />
            </div>
          ) : (
            <div className="notes-panel-empty">
              <Sparkles size={22} strokeWidth={1.5} />
              <p>{t("notes.summaryEmpty")}</p>
              <p className="notes-panel-hint">{t("notes.summaryHint")}</p>
            </div>
          )}
        </div>
      )}
    </section>
  );
}
