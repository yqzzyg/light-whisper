import { fireEvent, render, screen } from "@testing-library/react";
import { Editor } from "@tiptap/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("react-i18next", () => ({
  useTranslation: () => ({ i18n: { language: "zh" }, t: (key: string) => key }),
}));

import NoteEditorToolbar from "@/components/notes/NoteEditorToolbar";
import { buildNoteExtensions, getEditorMarkdown } from "@/components/notes/RichTextEditor";

let editor: Editor;

function markdown(): string {
  return getEditorMarkdown(editor);
}

beforeEach(() => {
  editor = new Editor({
    element: document.createElement("div"),
    extensions: buildNoteExtensions(() => ""),
    content: "需要加粗的一行",
  });
});

afterEach(() => {
  editor.destroy();
});

describe("笔记格式工具栏", () => {
  it("点加粗把选中文字变成 **…**", () => {
    const { unmount } = render(<NoteEditorToolbar editor={editor} />);
    editor.commands.selectAll();
    fireEvent.click(screen.getByRole("button", { name: "notes.mdToolbar.bold" }));

    expect(markdown()).toBe("**需要加粗的一行**");
    unmount();
  });

  it("没有内容时先插标题再输入，落库是一级标题", () => {
    editor.commands.clearContent();
    const { unmount } = render(<NoteEditorToolbar editor={editor} />);
    fireEvent.click(screen.getByRole("button", { name: "notes.mdToolbar.h1" }));
    editor.commands.insertContent("小节");

    expect(markdown()).toBe("# 小节");
    unmount();
  });

  it("按钮的按下态跟着光标位置走", () => {
    editor.commands.setContent("# 已经是标题");
    editor.commands.setTextSelection(3);
    const { unmount } = render(<NoteEditorToolbar editor={editor} />);

    expect(screen.getByRole("button", { name: "notes.mdToolbar.h1" })).toHaveAttribute(
      "aria-pressed",
      "true",
    );
    expect(screen.getByRole("button", { name: "notes.mdToolbar.h2" })).toHaveAttribute(
      "aria-pressed",
      "false",
    );
    unmount();
  });

  it("插表格后出现表格操作按钮", () => {
    editor.commands.clearContent();
    const { unmount } = render(<NoteEditorToolbar editor={editor} />);
    expect(screen.queryByRole("button", { name: "notes.mdToolbar.addRow" })).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "notes.mdToolbar.insertTable" }));
    expect(screen.getByRole("button", { name: "notes.mdToolbar.addRow" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "notes.mdToolbar.deleteTable" })).toBeInTheDocument();
    unmount();
  });

  it("点击按钮时不让编辑器失焦", () => {
    const { unmount } = render(<NoteEditorToolbar editor={editor} />);
    const button = screen.getByRole("button", { name: "notes.mdToolbar.bold" });
    const event = new MouseEvent("mousedown", { bubbles: true, cancelable: true });
    button.dispatchEvent(event);

    // 默认行为被阻止，ProseMirror 的选区才不会在 click 之前被清掉。
    expect(event.defaultPrevented).toBe(true);
    unmount();
  });
});
