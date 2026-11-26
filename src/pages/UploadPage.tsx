import { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { ArrowLeft, FileAudio, LoaderCircle, Minus, NotebookPen, Upload, X } from "lucide-react";
import { getCurrentWindow } from "@tauri-apps/api/window";

import { copyToClipboard, hideMainWindow } from "@/api/tauri";
import TitleBar from "@/components/TitleBar";
import TranscriptionResult from "@/components/TranscriptionResult";
import { useAudioImport } from "@/hooks/useAudioImport";
import { createNoteFromHistory } from "@/hooks/notesStore";

type View = "main" | "settings" | "history" | "upload" | "notes";

/** 取路径里最后一段，Windows 与 POSIX 分隔符都要认。 */
function fileNameOf(path: string): string {
  return path.split(/[\\/]/).pop() || path;
}

/** 时长按 `分:秒` 显示；容器给不出帧数时退回破折号。 */
function formatDuration(seconds: number | null): string {
  if (seconds == null || !Number.isFinite(seconds) || seconds <= 0) return "—";
  const total = Math.round(seconds);
  return `${Math.floor(total / 60)}:${String(total % 60).padStart(2, "0")}`;
}

function formatSize(bytes: number): string {
  if (bytes >= 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  return `${Math.max(1, Math.round(bytes / 1024))} KB`;
}

export default function UploadPage({
  onNavigate,
  animClass = "",
}: {
  onNavigate: (view: View) => void;
  animClass?: string;
}) {
  const { t } = useTranslation();
  const { isImporting, picked, result, segments, fallbackReason, selectAudio, startImport, cancelSelection }
    = useAudioImport();
  const [copiedId, setCopiedId] = useState<string | null>(null);
  const copyTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  // 转写期间没有逐字文本（上传与笔记走同一条切片/整段路径），转完才有结果卡。
  const finalResult = result !== null && result.status !== "asr_error" && !!result.text;
  const hasResult = finalResult;
  const previewText = finalResult ? result.text : null;
  // 已选好文件、尚未开始：这一段显示文件信息与「开始转写」。
  const awaitingConfirm = picked !== null && !isImporting && !hasResult;

  useEffect(() => () => {
    if (copyTimerRef.current) clearTimeout(copyTimerRef.current);
  }, []);

  const handleCopy = useCallback(async (text: string, id: string) => {
    try {
      await copyToClipboard(text);
      setCopiedId(id);
      toast.success(t("common.copiedToClipboard"));
      if (copyTimerRef.current) clearTimeout(copyTimerRef.current);
      copyTimerRef.current = setTimeout(() => {
        setCopiedId(null);
        copyTimerRef.current = null;
      }, 1500);
    } catch {
      toast.error(t("common.copyFailed"));
    }
  }, [t]);

  /** 存为笔记：导入结果已写入历史，笔记只带走转录与时长，然后直接跳过去。 */
  const handleSaveAsNote = useCallback(async () => {
    if (!result) return;
    try {
      await createNoteFromHistory(result.id);
      toast.success(t("upload.saveToNotesDone"));
      onNavigate("notes");
    } catch (error) {
      toast.error(t("upload.saveToNotesFailed", { error: error instanceof Error ? error.message : String(error) }));
    }
  }, [onNavigate, result, t]);

  const selectButtonLabel = isImporting
    ? "upload.importing"
    : hasResult
      ? "upload.importAnother"
      : "upload.selectFile";

  return (
    <div className={`page-root upload-page ${animClass}`}>
      <TitleBar
        title={t("upload.title")}
        leftAction={(
          <button className="icon-btn plain" aria-label={t("common.back")} onClick={() => onNavigate("main")}>
            <ArrowLeft size={14} />
          </button>
        )}
        rightActions={(
          <>
            <button aria-label={t("common.minimize")} className="icon-btn" onClick={() => getCurrentWindow().minimize()}>
              <Minus size={12} />
            </button>
            <button aria-label={t("common.close")} className="icon-btn" onClick={() => hideMainWindow()}>
              <X size={12} />
            </button>
          </>
        )}
      />
      <div className={`page-content upload-content${hasResult ? " upload-content-with-result" : ""}`}>
        <div className="upload-card">
          <div className="upload-card-icon" aria-hidden="true">
            {isImporting
              ? <LoaderCircle size={30} className="animate-spin" />
              : picked
                ? <FileAudio size={30} strokeWidth={1.5} />
                : <Upload size={30} strokeWidth={1.5} />}
          </div>
          <div className="upload-card-title">
            {t(isImporting ? "upload.importing" : awaitingConfirm ? "upload.pickedTitle" : "upload.cardTitle")}
          </div>

          {awaitingConfirm ? (
            <>
              <div className="upload-picked-name" title={picked.path}>{fileNameOf(picked.path)}</div>
              <div className="upload-picked-meta">
                <span>{formatDuration(picked.probe.durationSec)}</span>
                <span aria-hidden="true">·</span>
                <span>{formatSize(picked.probe.sizeBytes)}</span>
                <span aria-hidden="true">·</span>
                <span>{picked.probe.engine}</span>
              </div>
              {picked.probe.limitHint && (
                <p className="upload-picked-hint is-warning">
                  {picked.probe.limitHint}
                </p>
              )}
              <div className="upload-picked-actions">
                <button
                  type="button"
                  className="upload-select-btn"
                  onClick={() => void startImport()}
                  disabled={picked.probe.limitHint !== null}
                >
                  <FileAudio size={14} aria-hidden="true" />
                  <span>{t("upload.startTranscribe")}</span>
                </button>
                <button type="button" className="upload-ghost-btn" onClick={cancelSelection}>
                  {t("upload.chooseAnother")}
                </button>
              </div>
            </>
          ) : (
            <>
              <p className="upload-card-hint">{t("upload.hint")}</p>
              <button
                type="button"
                className="upload-select-btn"
                onClick={() => void selectAudio()}
                disabled={isImporting}
                aria-busy={isImporting}
              >
                {isImporting
                  ? <LoaderCircle size={14} className="animate-spin" aria-hidden="true" />
                  : <FileAudio size={14} aria-hidden="true" />}
                <span>{t(selectButtonLabel)}</span>
              </button>
            </>
          )}
        </div>

        {hasResult && previewText && (
          <div className="upload-result animate-slide-up">
            <TranscriptionResult
              text={previewText}
              originalText={result?.originalText ?? null}
              isProcessing={false}
              copiedId={copiedId}
              onCopy={handleCopy}
              durationSec={result?.durationSec ?? null}
              charCount={previewText.length}
              detectedLanguage={result?.language ?? null}
              timing={{
                asrMs: result?.asrMs ?? undefined,
                polishMs: result?.polishMs ?? undefined,
                totalMs: result?.totalMs ?? undefined,
              }}
              segments={segments}
              fallbackReason={fallbackReason}
            />
            <p className="upload-result-hint">{t("upload.resultHint")}</p>
            {finalResult && (
              <button type="button" className="upload-save-note-btn" onClick={() => void handleSaveAsNote()}>
                <NotebookPen size={13} aria-hidden="true" />
                <span>{t("upload.saveToNotes")}</span>
              </button>
            )}
          </div>
        )}
      </div>
    </div>
  );
}
