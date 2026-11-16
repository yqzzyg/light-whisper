import { useRef, useCallback, useEffect, useLayoutEffect, useState, type UIEvent } from "react";
import { useTranslation } from "react-i18next";
import { Loader2, Copy, Check, ChevronDown, ChevronUp } from "lucide-react";
import type { EditGrabStatus, TranscriptionTiming } from "@/types";

interface TranscriptionResultProps {
  text: string | null;
  originalText: string | null;
  isProcessing: boolean;
  copiedId: string | null;
  onCopy: (text: string, id: string) => void;
  onDraftChange?: (newText: string) => void;
  onTextChange?: (newText: string) => void;
  durationSec: number | null;
  charCount: number | null;
  detectedLanguage?: string | null;
  editGrabStatus?: EditGrabStatus | null;
  timing?: TranscriptionTiming | null;
}

export default function TranscriptionResult({
  text,
  originalText,
  isProcessing,
  copiedId,
  onCopy,
  onDraftChange,
  onTextChange,
  durationSec,
  charCount,
  detectedLanguage,
  editGrabStatus,
  timing,
}: TranscriptionResultProps) {
  const { t } = useTranslation();
  const bodyRef = useRef<HTMLTextAreaElement>(null);
  const [draftText, setDraftText] = useState(text ?? "");
  const [expanded, setExpanded] = useState(false);
  const [isOverflowing, setIsOverflowing] = useState(false);
  const [reachedBodyEnd, setReachedBodyEnd] = useState(false);
  const hasResult = text !== null;
  const hasStats = !!text && durationSec && durationSec > 0 && charCount;
  const editGrabHintKey =
    editGrabStatus === "timeout" || editGrabStatus === "empty"
      ? `result.editGrab.${editGrabStatus}`
      : null;
  const latencyParts = [
    timing?.asrMs != null ? t("result.latency.asr", { ms: timing.asrMs }) : null,
    timing?.polishMs != null ? t("result.latency.ai", { ms: timing.polishMs }) : null,
    timing?.totalMs != null ? t("result.latency.total", { ms: timing.totalMs }) : null,
  ].filter((part): part is string => !!part);
  const showMeta = hasStats || editGrabHintKey || latencyParts.length > 0;

  useEffect(() => {
    setDraftText(text ?? "");
  }, [text]);

  const handleChange = useCallback((newText: string) => {
    setDraftText(newText);
    onDraftChange?.(newText);
  }, [onDraftChange]);

  // textarea 的 rows 只按显式换行计算，长文本自动折行后高度不够。
  // 按实际 scrollHeight 伸展，但由 CSS max-height 封顶；超出部分在卡内滚动，
  // 并显示「展开全部」，避免长内容把整个页面撑高。
  const resizeBody = useCallback(() => {
    const el = bodyRef.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${el.scrollHeight}px`;
    // 折叠态下高度被 CSS max-height 截断，scrollHeight 仍反映完整内容；
    // 据此判断是否需要显示「展开全部」。
    setIsOverflowing(el.scrollHeight > el.clientHeight + 2);
    setReachedBodyEnd(el.scrollTop + el.clientHeight >= el.scrollHeight - 2);
  }, []);

  useLayoutEffect(() => {
    if (hasResult) resizeBody();
  }, [draftText, expanded, hasResult, resizeBody]);

  const handleToggleExpanded = useCallback(() => {
    setExpanded((prev) => !prev);
    setReachedBodyEnd(false);
    if (bodyRef.current) bodyRef.current.scrollTop = 0;
  }, []);

  const handleBodyScroll = useCallback((event: UIEvent<HTMLTextAreaElement>) => {
    const el = event.currentTarget;
    setReachedBodyEnd(el.scrollTop + el.clientHeight >= el.scrollHeight - 2);
  }, []);

  // 窗口或容器宽度变化会改变折行数，需要重新计算高度。
  useEffect(() => {
    const el = bodyRef.current;
    if (!el || !hasResult) return;
    let lastWidth = el.clientWidth;
    const handleWidthChange = () => {
      if (el.clientWidth !== lastWidth) {
        lastWidth = el.clientWidth;
        resizeBody();
      }
    };
    window.addEventListener("resize", handleWidthChange);
    const observer = typeof ResizeObserver !== "undefined" ? new ResizeObserver(handleWidthChange) : null;
    observer?.observe(el);
    return () => {
      window.removeEventListener("resize", handleWidthChange);
      observer?.disconnect();
    };
  }, [hasResult, resizeBody]);

  const handleBlur = useCallback(() => {
    const edited = draftText.trim();
    const baseline = originalText?.trim() ?? "";
    if (edited && baseline && edited !== baseline) {
      onTextChange?.(edited);
    }
  }, [draftText, originalText, onTextChange]);

  const handleCopy = useCallback(() => {
    const currentText = bodyRef.current?.value.trim() ?? draftText.trim() ?? text ?? "";
    onCopy(currentText, "original");
  }, [draftText, text, onCopy]);

  return (
    <>
      {hasResult && (
        <div style={{ marginBottom: 12 }} className="animate-slide-up">
          <div className={`result-card${isProcessing ? " result-card-busy" : ""}`} aria-busy={isProcessing}>
            <div className="result-card-header">
              <span className="result-card-title">
                <span className="result-dot" />
                {t("result.title")}
              </span>
              <div className="result-card-header-actions">
                {expanded && (
                  <button
                    type="button"
                    className="result-header-toggle"
                    onClick={handleToggleExpanded}
                    aria-expanded="true"
                  >
                    <ChevronUp size={11} aria-hidden="true" />
                    <span>{t("result.collapse")}</span>
                  </button>
                )}
                <button aria-label={t("common.copy")} className="icon-btn icon-btn-sm" onClick={handleCopy}>
                  {copiedId === "original"
                    ? <span className="animate-check-draw"><Check size={12} /></span>
                    : <Copy size={12} strokeWidth={1.5} />}
                </button>
              </div>
            </div>
            {isProcessing && (
              <div className="result-processing" role="status">
                <Loader2 size={12} className="animate-spin" />
                <span>{t("result.recognizingSpeech")}</span>
                <span className="skeleton-shimmer result-processing-line" />
              </div>
            )}
            <div className="result-body-wrap">
              <textarea
                ref={bodyRef}
                className={`result-card-body${expanded ? " result-card-body-expanded" : ""}`}
                aria-label={t("result.editableTranscription")}
                value={draftText}
                rows={Math.max(3, draftText.split(/\r?\n/).length)}
                onChange={(event) => handleChange(event.target.value)}
                onBlur={handleBlur}
                onScroll={handleBodyScroll}
                spellCheck={false}
              />
              {!expanded && isOverflowing && !reachedBodyEnd && (
                <span className="result-body-fade" aria-hidden="true" />
              )}
            </div>
            {!expanded && isOverflowing && (
              <div className="result-body-actions">
                <button
                  type="button"
                  className="result-body-toggle"
                  onClick={handleToggleExpanded}
                  aria-expanded="false"
                >
                  <ChevronDown size={12} aria-hidden="true" />
                  <span>{t("result.expandAll")}</span>
                </button>
              </div>
            )}
            {showMeta && (
              <p className="result-card-stats">
                {hasStats && detectedLanguage && (
                  <span className="result-lang-tag">{detectedLanguage}</span>
                )}
                {hasStats && t("result.stats", { chars: charCount, duration: durationSec.toFixed(1), cpm: Math.round((charCount / durationSec) * 60) })}
                {latencyParts.length > 0 && (
                  <span className="result-latency">{latencyParts.join(" · ")}</span>
                )}
                {editGrabHintKey && (
                  <span className="result-edit-grab-hint">{t(editGrabHintKey)}</span>
                )}
              </p>
            )}
          </div>
        </div>
      )}
      {isProcessing && !text && (
        <div className="animate-fade-in" style={{ marginBottom: 12 }}>
          <div className="result-card result-card-skeleton">
            <div className="skeleton-card-header">
              <Loader2 size={12} className="animate-spin icon-tertiary" strokeWidth={1.75} />
              <span>{t("result.recognizingSpeech")}</span>
            </div>
            <div className="skeleton-paragraph">
              <div className="skeleton-shimmer skeleton-line" style={{ width: "94%" }} />
              <div className="skeleton-shimmer skeleton-line" style={{ width: "78%" }} />
              <div className="skeleton-shimmer skeleton-line" style={{ width: "62%" }} />
            </div>
          </div>
        </div>
      )}
    </>
  );
}
