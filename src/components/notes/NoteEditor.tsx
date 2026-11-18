import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import {
  Copy,
  Download,
  FileText,
  LoaderCircle,
  Sparkles,
  Trash2,
  X,
} from "lucide-react";

import { copyToClipboard, saveTextFile } from "@/api/tauri";
import MarkdownContent from "@/components/MarkdownContent";
import MarkdownEditor from "@/components/notes/MarkdownEditor";
import type { NoteDetail, NoteFolder, NotePatch } from "@/types/notes";

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
  onEdit,
  onFlush,
  onMove,
  onDelete,
  onClose,
}: NoteEditorProps) {
  const { t, i18n } = useTranslation();
  const [tab, setTab] = useState<Tab>("content");

  // 换一条笔记时回到正文 tab：上一条停在哪一页对这条没有意义。
  useEffect(() => {
    setTab("content");
  }, [note?.id]);

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
          {transcript.trim() && <span className="notes-tab-dot" aria-hidden="true" />}
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
          value={note.content}
          placeholder={t("notes.contentPlaceholder")}
          onChange={(content) => onEdit({ content })}
          onBlur={onFlush}
        />
      )}

      {tab === "transcript" && (
        <div className="notes-panel">
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
          <div className="notes-toolbar">
            <span className="notes-toolbar-spacer" />
            <button
              type="button"
              className="notes-toolbar-btn"
              disabled={!summary.trim()}
              onClick={() => void exportNote("markdown")}
            >
              <Download size={12} />
              {t("notes.exportMarkdown")}
            </button>
          </div>
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
