import { useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { Download } from "lucide-react";

import MarkdownContent from "@/components/MarkdownContent";
import RichTextEditor from "@/components/notes/RichTextEditor";

export interface MarkdownEditorProps {
  value: string;
  placeholder?: string;
  onChange: (value: string) => void;
  onBlur?: () => void;
  /** 把整条笔记导出成 markdown；不传就不显示导出按钮。 */
  onExport?: () => void;
  /** 整条笔记一个字都没有时禁用导出，避免落下一个空文件。 */
  exportDisabled?: boolean;
}

type Mode = "rich" | "source" | "preview";

/**
 * 笔记正文编辑区，三态切换：所见即所得 / markdown 源码 / 只读预览。
 *
 * 三态同一时刻只挂一个：`RichTextEditor` 会建 ProseMirror 文档、
 * `MarkdownContent` 会跑 remark + katex，都没必要在后台常驻。
 * 来回切不丢内容，因为 `value` 始终是唯一事实源（store 里的自动保存）。
 */
export default function MarkdownEditor({
  value,
  placeholder,
  onChange,
  onBlur,
  onExport,
  exportDisabled = false,
}: MarkdownEditorProps) {
  const { t } = useTranslation();
  const [mode, setMode] = useState<Mode>("rich");
  const charCount = useMemo(() => value.trim().length, [value]);

  const modes: Array<{ key: Mode; label: string }> = [
    { key: "rich", label: t("notes.edit") },
    { key: "source", label: t("notes.source") },
    { key: "preview", label: t("notes.preview") },
  ];

  return (
    <div className="notes-md">
      <div className="notes-md-bar">
        <div className="notes-md-toggle">
          {modes.map((item) => (
            <button
              key={item.key}
              type="button"
              className="notes-md-toggle-btn"
              data-active={mode === item.key}
              aria-pressed={mode === item.key}
              onClick={() => setMode(item.key)}
            >
              {item.label}
            </button>
          ))}
        </div>
        <div className="notes-md-bar-right">
          {onExport && (
            <button
              type="button"
              className="notes-toolbar-btn"
              title={t("notes.exportMarkdown")}
              disabled={exportDisabled}
              onClick={onExport}
            >
              <Download size={12} />
              {t("notes.exportMarkdown")}
            </button>
          )}
          <span className="notes-md-count">{t("notes.charCount", { count: charCount })}</span>
        </div>
      </div>

      {mode === "rich" && (
        <RichTextEditor
          value={value}
          placeholder={placeholder}
          onChange={onChange}
          onBlur={onBlur}
        />
      )}

      {mode === "source" && (
        <textarea
          className="notes-md-input"
          value={value}
          placeholder={placeholder}
          spellCheck={false}
          onChange={(event) => onChange(event.target.value)}
          onBlur={onBlur}
        />
      )}

      {mode === "preview" &&
        (value.trim() ? (
          <div className="notes-md-preview">
            <MarkdownContent content={value} />
          </div>
        ) : (
          <div className="notes-panel-empty">
            <p>{t("notes.previewEmpty")}</p>
          </div>
        ))}
    </div>
  );
}
