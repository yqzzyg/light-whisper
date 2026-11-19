import { useEffect, useRef } from "react";
import { EditorContent, Extension, useEditor, type Editor } from "@tiptap/react";
import StarterKit from "@tiptap/starter-kit";
import { Table, TableCell, TableHeader, TableRow } from "@tiptap/extension-table";
import TaskItem from "@tiptap/extension-task-item";
import TaskList from "@tiptap/extension-task-list";
import Placeholder from "@tiptap/extension-placeholder";
import CodeBlockLowlight from "@tiptap/extension-code-block-lowlight";
import { common, createLowlight } from "lowlight";
import { Markdown } from "tiptap-markdown";

import NoteEditorToolbar from "@/components/notes/NoteEditorToolbar";

/** 代码块高亮：只装 highlight.js 的 common 语言集，不引全量语法包。 */
const lowlight = createLowlight(common);

/**
 * 单元格里禁止拆段。
 *
 * GFM 管道表的每个单元格只能是「一行」，而 `tiptap-markdown` 的表格序列化在
 * 遇到多段落单元格时会直接退化成 `[table]`——整张表的内容就此消失。
 * 所以回车不拆段，改成在当前行下面加一行，行为可预期且不丢数据。
 */
const TableEnterGuard = Extension.create({
  name: "tableEnterGuard",
  priority: 100,
  addKeyboardShortcuts() {
    const addRow = () => {
      if (!this.editor.isActive("table")) return false;
      return this.editor.commands.addRowAfter();
    };
    return { Enter: addRow, "Shift-Enter": addRow };
  },
});

export interface RichTextEditorProps {
  value: string;
  placeholder?: string;
  onChange: (value: string) => void;
  onBlur?: () => void;
}

/**
 * 取出编辑器的 markdown 序列化结果。
 *
 * `tiptap-markdown` 没有给 `Editor` 补类型，只能在这里收口一次，
 * 别让 `as any` 散到调用点。
 */
export function getEditorMarkdown(editor: Editor): string {
  const storage = editor.storage as unknown as { markdown?: { getMarkdown(): string } };
  return storage.markdown?.getMarkdown() ?? "";
}

/**
 * 笔记正文的扩展组合。
 *
 * 单独导出是为了让「markdown 往返」测试复用同一份配置——
 * 测试里另抄一份扩展列表，就会在真正的编辑器改配置后悄悄失去意义。
 */
export function buildNoteExtensions(placeholder: () => string) {
  return [
    StarterKit.configure({
      // 用带高亮的代码块替换默认实现。
      codeBlock: false,
      heading: { levels: [1, 2, 3] },
      link: {
        // 编辑时点链接不该跳走，交给预览态。
        openOnClick: false,
        autolink: true,
        HTMLAttributes: { rel: "noreferrer", target: "_blank" },
      },
    }),
    CodeBlockLowlight.configure({ lowlight }),
    Table.configure({ resizable: false }),
    TableRow,
    TableHeader,
    TableCell,
    TableEnterGuard,
    TaskList,
    TaskItem.configure({ nested: true }),
    Placeholder.configure({ placeholder }),
    Markdown.configure({
      // 笔记里不渲染内联 HTML（与只读渲染的 skipHtml 保持一致）。
      html: false,
      tightLists: true,
      linkify: true,
      // 单个换行是「换行」不是「空格」：只读渲染给 p 加了 white-space: pre-wrap，
      // 源码里的软换行本来就是看得见的，编辑器必须保持同一语义。
      breaks: true,
      transformPastedText: true,
      transformCopiedText: true,
    }),
  ];
}

/**
 * 所见即所得的 Markdown 编辑器（TipTap / ProseMirror）。
 *
 * 对外只认 markdown 字符串，内部是 ProseMirror 文档模型，两边的同步规则：
 *
 * - **向外**：`onUpdate` 里序列化后回写，同时记进 `serializedRef`；
 * - **向内**：只有外部值确实变化（且不等于上次序列化结果）才 `setContent`，
 *   否则每次保存回流都会重建文档、把光标顶到开头。
 *
 * 换笔记由调用方的 `key` 负责重建，不在这里做 `note.id` 判断。
 */
export default function RichTextEditor({
  value,
  placeholder,
  onChange,
  onBlur,
}: RichTextEditorProps) {
  /** 上一次序列化/加载的 markdown，用来识别「外部值带回来的回声」。 */
  const serializedRef = useRef(value);
  /** 外部写入文档期间屏蔽 `onUpdate`，避免把回灌当成用户编辑再存一次。 */
  const suppressRef = useRef(false);
  // 回调与文案走 ref：`useEditor` 的 options 只在首次创建时固定，直接闭包会拿到旧引用。
  const onChangeRef = useRef(onChange);
  const onBlurRef = useRef(onBlur);
  const placeholderRef = useRef(placeholder);
  useEffect(() => {
    onChangeRef.current = onChange;
    onBlurRef.current = onBlur;
    placeholderRef.current = placeholder;
  });

  const editor = useEditor({
    extensions: buildNoteExtensions(() => placeholderRef.current ?? ""),
    content: value,
    editorProps: {
      attributes: {
        // markdown-content 复用只读渲染的排版，所见即所得才和预览一致。
        class: "notes-rt-content markdown-content",
        spellcheck: "false",
        "aria-label": placeholder ?? "",
      },
    },
    onUpdate: ({ editor: current, transaction }) => {
      // 插件追加的空段落不是用户编辑，存下来还会污染正文。
      if (suppressRef.current || !transaction.docChanged) return;
      const markdown = getEditorMarkdown(current);
      serializedRef.current = markdown;
      onChangeRef.current(markdown);
    },
    onBlur: () => onBlurRef.current?.(),
  });

  useEffect(() => {
    if (!editor || editor.isDestroyed) return;
    if (value === serializedRef.current) return;
    // 空笔记会被后端按空串回写，这个回声不该重置文档。
    if (!value && !serializedRef.current.trim()) return;

    serializedRef.current = value;
    suppressRef.current = true;
    try {
      const { from, to } = editor.state.selection;
      editor.commands.setContent(value);
      // 尽量守住光标：位置越界时夹到新文档长度内，别让它抛异常。
      const limit = editor.state.doc.content.size;
      editor.commands.setTextSelection({ from: Math.min(from, limit), to: Math.min(to, limit) });
    } finally {
      suppressRef.current = false;
    }
  }, [value, editor]);

  return (
    <div className="notes-rt">
      <NoteEditorToolbar editor={editor} />
      <EditorContent editor={editor} className="notes-rt-scroll" />
    </div>
  );
}
