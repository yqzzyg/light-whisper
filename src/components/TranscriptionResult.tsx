import { useRef, useCallback, useEffect, useLayoutEffect, useState, type UIEvent } from "react";
import { useTranslation } from "react-i18next";
import { Loader2, Copy, Check, ChevronDown, ChevronUp } from "lucide-react";
import SpeakerSegments from "@/components/SpeakerSegments";
import { formatLatencySeconds } from "@/lib/duration";
import type {
  DiarizationFallbackReason,
  EditGrabStatus,
  HistorySpeakerSegment,
  TranscriptionTiming,
} from "@/types";

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
  /**
   * 说话人分段。空表或不传就是「这次没分离」，卡片只显示正文，不显示视图切换。
   */
  segments?: HistorySpeakerSegment[];
  /** 没做分离的原因；有值时在卡片底部补一句为什么没有说话人。 */
  fallbackReason?: DiarizationFallbackReason | null;
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
  segments,
  fallbackReason,
}: TranscriptionResultProps) {
  const { t } = useTranslation();
  const bodyRef = useRef<HTMLTextAreaElement>(null);
  const [draftText, setDraftText] = useState(text ?? "");
  const [expanded, setExpanded] = useState(false);
  const [isOverflowing, setIsOverflowing] = useState(false);
  const [reachedBodyEnd, setReachedBodyEnd] = useState(false);
  const hasSpeakers = (segments?.length ?? 0) > 0;
  // 默认给说话人视图：用户做分离就是为了看这个。没有分段时恒为正文。
  const [view, setView] = useState<"speaker" | "text">(hasSpeakers ? "speaker" : "text");
  const hasResult = text !== null;
  const hasStats = !!text && durationSec && durationSec > 0 && charCount;
  const editGrabHintKey =
    editGrabStatus === "timeout" || editGrabStatus === "empty"
      ? `result.editGrab.${editGrabStatus}`
      : null;
  // `disabled` 是用户自己关的，不用再解释一遍；其余原因都值得说明。
  const fallbackHintKey =
    fallbackReason && fallbackReason !== "disabled"
      ? `result.diarizationFallback.${fallbackReason}`
      : null;
  const latencyParts = [
    { key: "result.latency.asr", ms: timing?.asrMs },
    { key: "result.latency.ai", ms: timing?.polishMs },
    { key: "result.latency.total", ms: timing?.totalMs },
  ]
    .map(({ key, ms }) => {
      const seconds = formatLatencySeconds(ms);
      return seconds === null ? null : t(key, { seconds });
    })
    .filter((part): part is string => !!part);
  const showMeta = hasStats || editGrabHintKey || latencyParts.length > 0;

  useEffect(() => {
    setDraftText(text ?? "");
  }, [text]);

  // 重新转写后分段可能消失（降级、换了引擎、或关掉了开关），那就没有「说话人」
  // 可看，必须切回正文，否则会停在一个空白视图上。
  useEffect(() => {
    if (!hasSpeakers) setView("text");
  }, [hasSpeakers]);

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
    // 说话人视图复制「说话人 N：文本」的整段，与历史里存的正文一致；
    // 正文视图复制用户当前编辑的内容（可能是手改过的草稿）。
    if (view === "speaker" && hasSpeakers) {
      const spoken = (segments ?? [])
        .filter((segment) => segment.text.trim())
        .map(
          (segment) =>
            `${t("notes.speaker", { index: segment.speakerId + 1 })}：${segment.text.trim()}`,
        )
        .join("\n");
      onCopy(spoken, "original");
      return;
    }
    const currentText = bodyRef.current?.value.trim() ?? draftText.trim() ?? text ?? "";
    onCopy(currentText, "original");
  }, [view, hasSpeakers, segments, t, draftText, text, onCopy]);

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
                {hasSpeakers && (
                  // 只有存在分段时才给切换：没分段时两个视图内容完全一样，
                  // 多一个按钮只会让人以为点坏了。
                  <div className="result-view-switch" role="group">
                    <button
                      type="button"
                      className="result-view-btn"
                      aria-pressed={view === "speaker"}
                      onClick={() => setView("speaker")}
                    >
                      {t("result.speakerView")}
                    </button>
                    <button
                      type="button"
                      className="result-view-btn"
                      aria-pressed={view === "text"}
                      onClick={() => setView("text")}
                    >
                      {t("result.textView")}
                    </button>
                  </div>
                )}
                {expanded && view === "text" && (
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
            {view === "speaker" && hasSpeakers ? (
              <div className="result-speaker-body">
                <SpeakerSegments segments={segments ?? []} />
              </div>
            ) : (
              <>
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
              </>
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
            {fallbackHintKey && (
              // 降级不是错误：转写成功了，只是没有分段。所以用提示行而不是报错。
              <p className="result-speaker-fallback">{t(fallbackHintKey)}</p>
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
