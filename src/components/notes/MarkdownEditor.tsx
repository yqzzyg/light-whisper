import { useMemo, useState } from "react";
import { useTranslation } from "react-i18next";

import MarkdownContent from "@/components/MarkdownContent";

export interface MarkdownEditorProps {
  value: string;
  placeholder?: string;
  onChange: (value: string) => void;
  onBlur?: () => void;
}

/**
 * Markdown 源码 + 预览切换。
 *
 * 预览只在切到预览态时挂载 `MarkdownContent`——它带数学公式与代码块的预处理，
 * 编辑时不需要为每个按键付这份解析开销。
 */
export default function MarkdownEditor({
  value,
  placeholder,
  onChange,
  onBlur,
}: MarkdownEditorProps) {
  const { t } = useTranslation();
  const [mode, setMode] = useState<"edit" | "preview">("edit");
  const charCount = useMemo(() => value.trim().length, [value]);

  return (
    <div className="notes-md">
      <div className="notes-md-bar">
        <div className="notes-md-toggle">
          <button
            type="button"
            className="notes-md-toggle-btn"
            data-active={mode === "edit"}
            onClick={() => setMode("edit")}
          >
            {t("notes.edit")}
          </button>
          <button
            type="button"
            className="notes-md-toggle-btn"
            data-active={mode === "preview"}
            onClick={() => setMode("preview")}
          >
            {t("notes.preview")}
          </button>
        </div>
        <span className="notes-md-count">{t("notes.charCount", { count: charCount })}</span>
      </div>

      {mode === "edit" ? (
        <textarea
          className="notes-md-input"
          value={value}
          placeholder={placeholder}
          spellCheck={false}
          onChange={(event) => onChange(event.target.value)}
          onBlur={onBlur}
        />
      ) : value.trim() ? (
        <div className="notes-md-preview">
          <MarkdownContent content={value} />
        </div>
      ) : (
        <div className="notes-panel-empty">
          <p>{t("notes.previewEmpty")}</p>
        </div>
      )}
    </div>
  );
}
