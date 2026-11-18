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

export default function UploadPage({
  onNavigate,
  animClass = "",
}: {
  onNavigate: (view: View) => void;
  animClass?: string;
}) {
  const { t } = useTranslation();
  const { isImporting, result, interimText, importAudio } = useAudioImport();
  const [copiedId, setCopiedId] = useState<string | null>(null);
  const copyTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  // 导入中：本地引擎边转边推 interim，用同一张结果卡逐句显示；
  // 转完（或在线引擎）后切到最终记录。
  const isStreaming = isImporting && !!interimText;
  const finalResult = result !== null && result.status !== "asr_error" && !!result.text;
  const hasResult = isStreaming || finalResult;
  const previewText = isStreaming ? interimText : finalResult ? result.text : null;

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
            {isImporting ? <LoaderCircle size={30} className="animate-spin" /> : <Upload size={30} strokeWidth={1.5} />}
          </div>
          <div className="upload-card-title">{t(isImporting ? "upload.importing" : "upload.cardTitle")}</div>
          <p className="upload-card-hint">{t("upload.hint")}</p>
          <button
            type="button"
            className="upload-select-btn"
            onClick={() => void importAudio()}
            disabled={isImporting}
            aria-busy={isImporting}
          >
            {isImporting
              ? <LoaderCircle size={14} className="animate-spin" aria-hidden="true" />
              : <FileAudio size={14} aria-hidden="true" />}
            <span>{t(isImporting ? "upload.importing" : hasResult ? "upload.importAnother" : "upload.selectFile")}</span>
          </button>
        </div>

        {hasResult && previewText && (
          <div className="upload-result animate-slide-up">
            <TranscriptionResult
              text={previewText}
              originalText={isStreaming ? null : (result?.originalText ?? null)}
              isProcessing={isStreaming}
              copiedId={copiedId}
              onCopy={handleCopy}
              durationSec={isStreaming ? null : (result?.durationSec ?? null)}
              charCount={previewText.length}
              detectedLanguage={isStreaming ? null : (result?.language ?? null)}
              timing={isStreaming ? undefined : {
                asrMs: result?.asrMs ?? undefined,
                polishMs: result?.polishMs ?? undefined,
                totalMs: result?.totalMs ?? undefined,
              }}
            />
            <p className="upload-result-hint">
              {t(isStreaming ? "upload.streamingHint" : "upload.resultHint")}
            </p>
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
