import type { ReactNode } from "react";
import { useEditorState, type Editor } from "@tiptap/react";
import {
  Bold,
  Code,
  Heading1,
  Heading2,
  Heading3,
  Italic,
  List,
  ListChecks,
  ListOrdered,
  Quote,
  Redo2,
  Rows3,
  Columns3,
  SquareCode,
  Strikethrough,
  Table2,
  Trash2,
  Undo2,
} from "lucide-react";
import { useTranslation } from "react-i18next";

export interface NoteEditorToolbarProps {
  editor: Editor | null;
}

interface ButtonSpec {
  label: string;
  icon: ReactNode;
  active?: boolean;
  disabled?: boolean;
  onClick: () => void;
}

/**
 * 笔记正文的固定工具栏。
 *
 * 只做「一行按钮」：标题、行内格式、列表、引用/代码、表格。
 * 浮动菜单、slash 命令、块拖拽留给后续按反馈补，不在 MVP 里预判。
 *
 * 所有按钮都 `type="button"` + `onMouseDown` 阻止默认行为，
 * 否则点击会先把编辑器选区清掉，格式就落不到刚才选中的文字上。
 */
export default function NoteEditorToolbar({ editor }: NoteEditorToolbarProps) {
  const { t } = useTranslation();

  // 只订阅必要的状态：每次事务都重算一遍全部 isActive 会让打字变卡。
  const state = useEditorState({
    editor,
    selector: ({ editor: current }) =>
      current
        ? {
            bold: current.isActive("bold"),
            italic: current.isActive("italic"),
            strike: current.isActive("strike"),
            code: current.isActive("code"),
            h1: current.isActive("heading", { level: 1 }),
            h2: current.isActive("heading", { level: 2 }),
            h3: current.isActive("heading", { level: 3 }),
            bulletList: current.isActive("bulletList"),
            orderedList: current.isActive("orderedList"),
            taskList: current.isActive("taskList"),
            blockquote: current.isActive("blockquote"),
            codeBlock: current.isActive("codeBlock"),
            inTable: current.isActive("table"),
            canUndo: current.can().undo(),
            canRedo: current.can().redo(),
          }
        : null,
  });

  if (!editor || !state) {
    return <div className="notes-rt-toolbar" aria-hidden="true" />;
  }

  const chain = () => editor.chain().focus();

  const groups: ButtonSpec[][] = [
    [
      {
        label: t("notes.mdToolbar.undo"),
        icon: <Undo2 size={14} />,
        disabled: !state.canUndo,
        onClick: () => chain().undo().run(),
      },
      {
        label: t("notes.mdToolbar.redo"),
        icon: <Redo2 size={14} />,
        disabled: !state.canRedo,
        onClick: () => chain().redo().run(),
      },
    ],
    [
      {
        label: t("notes.mdToolbar.h1"),
        icon: <Heading1 size={14} />,
        active: state.h1,
        onClick: () => chain().toggleHeading({ level: 1 }).run(),
      },
      {
        label: t("notes.mdToolbar.h2"),
        icon: <Heading2 size={14} />,
        active: state.h2,
        onClick: () => chain().toggleHeading({ level: 2 }).run(),
      },
      {
        label: t("notes.mdToolbar.h3"),
        icon: <Heading3 size={14} />,
        active: state.h3,
        onClick: () => chain().toggleHeading({ level: 3 }).run(),
      },
    ],
    [
      {
        label: t("notes.mdToolbar.bold"),
        icon: <Bold size={14} />,
        active: state.bold,
        onClick: () => chain().toggleBold().run(),
      },
      {
        label: t("notes.mdToolbar.italic"),
        icon: <Italic size={14} />,
        active: state.italic,
        onClick: () => chain().toggleItalic().run(),
      },
      {
        label: t("notes.mdToolbar.strike"),
        icon: <Strikethrough size={14} />,
        active: state.strike,
        onClick: () => chain().toggleStrike().run(),
      },
      {
        label: t("notes.mdToolbar.inlineCode"),
        icon: <Code size={14} />,
        active: state.code,
        onClick: () => chain().toggleCode().run(),
      },
    ],
    [
      {
        label: t("notes.mdToolbar.bulletList"),
        icon: <List size={14} />,
        active: state.bulletList,
        onClick: () => chain().toggleBulletList().run(),
      },
      {
        label: t("notes.mdToolbar.orderedList"),
        icon: <ListOrdered size={14} />,
        active: state.orderedList,
        onClick: () => chain().toggleOrderedList().run(),
      },
      {
        label: t("notes.mdToolbar.taskList"),
        icon: <ListChecks size={14} />,
        active: state.taskList,
        onClick: () => chain().toggleTaskList().run(),
      },
    ],
    [
      {
        label: t("notes.mdToolbar.blockquote"),
        icon: <Quote size={14} />,
        active: state.blockquote,
        onClick: () => chain().toggleBlockquote().run(),
      },
      {
        label: t("notes.mdToolbar.codeBlock"),
        icon: <SquareCode size={14} />,
        active: state.codeBlock,
        onClick: () => chain().toggleCodeBlock().run(),
      },
      // 表格相关的按钮只在光标落在表格里时出现，避免常驻一排用不上的开关。
      ...(state.inTable
        ? [
            {
              label: t("notes.mdToolbar.addRow"),
              icon: <Rows3 size={14} />,
              onClick: () => chain().addRowAfter().run(),
            },
            {
              label: t("notes.mdToolbar.addColumn"),
              icon: <Columns3 size={14} />,
              onClick: () => chain().addColumnAfter().run(),
            },
            {
              label: t("notes.mdToolbar.deleteTable"),
              icon: <Trash2 size={14} />,
              onClick: () => chain().deleteTable().run(),
            },
          ]
        : [
            {
              label: t("notes.mdToolbar.insertTable"),
              icon: <Table2 size={14} />,
              onClick: () => chain().insertTable({ rows: 3, cols: 3, withHeaderRow: true }).run(),
            },
          ]),
    ],
  ];

  return (
    <div className="notes-rt-toolbar" role="toolbar" aria-label={t("notes.mdToolbar.label")}>
      {groups.map((group, index) => (
        <div className="notes-rt-group" key={index}>
          {group.map((button) => (
            <button
              key={button.label}
              type="button"
              className="notes-rt-btn"
              data-active={button.active === true}
              title={button.label}
              aria-label={button.label}
              aria-pressed={button.active === undefined ? undefined : button.active}
              disabled={button.disabled}
              // 保住编辑器选区：mousedown 的默认行为会把焦点移走。
              onMouseDown={(event) => event.preventDefault()}
              onClick={button.onClick}
            >
              {button.icon}
            </button>
          ))}
        </div>
      ))}
    </div>
  );
}
