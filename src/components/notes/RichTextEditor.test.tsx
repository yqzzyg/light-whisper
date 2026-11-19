import { render } from "@testing-library/react";
import { Editor } from "@tiptap/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("react-i18next", () => ({
  useTranslation: () => ({ i18n: { language: "zh" }, t: (key: string) => key }),
}));

import RichTextEditor, {
  buildNoteExtensions,
  getEditorMarkdown,
} from "@/components/notes/RichTextEditor";

/** 用真实的扩展组合建一个游离编辑器，只测 markdown 进出。 */
function editorFor(markdown: string, element?: HTMLElement) {
  return new Editor({ element, extensions: buildNoteExtensions(() => ""), content: markdown });
}

const toMarkdown = getEditorMarkdown;

/** 「存进去 → 读出来 → 再存一次」，第二次必须和第一次一致（幂等）。 */
function roundTrip(markdown: string): string {
  const first = editorFor(markdown);
  const once = toMarkdown(first);
  first.destroy();
  const second = editorFor(once);
  const twice = toMarkdown(second);
  second.destroy();
  expect(twice).toBe(once);
  return once;
}

describe("笔记编辑器的 markdown 往返", () => {
  it("标题、强调、行内代码与链接保持不变", () => {
    const out = roundTrip(
      "## 结论\n\n本周**完成**三件事，含 `npm run build` 与 [文档](https://example.com)。\n",
    );
    expect(out).toContain("## 结论");
    expect(out).toContain("**完成**");
    expect(out).toContain("`npm run build`");
    expect(out).toContain("[文档](https://example.com)");
  });

  it("保留有序/无序列表的嵌套层级", () => {
    const out = roundTrip("- 第一项\n- 第二项\n  - 嵌套项\n\n1. 甲\n2. 乙\n");
    expect(out).toContain("- 第一项\n- 第二项\n  - 嵌套项");
    expect(out).toContain("1. 甲\n2. 乙");
  });

  it("保留待办列表的勾选状态", () => {
    // 已知差异：任务列表会被拆成「宽松列表」（项之间多一个空行），
    // 但勾选状态与顺序不变，GFM 渲染结果一致。
    const out = roundTrip("- [x] 已做\n- [ ] 未做\n");
    expect(out).toContain("[x] 已做");
    expect(out).toContain("[ ] 未做");
  });

  it("保留代码块的语言标记与内容", () => {
    const out = roundTrip("```ts\nconst a = 1;\n```\n");
    expect(out).toContain("```ts\nconst a = 1;\n```");
  });

  it("保留表格的行列结构与内容", () => {
    const out = roundTrip("| 项 | 值 |\n| --- | --- |\n| a | 1 |\n| b | 2 |\n");
    expect(out).toContain("| 项 | 值 |");
    expect(out).toContain("| a | 1 |");
    expect(out).toContain("| b | 2 |");
  });

  it("保留引用块与分隔线", () => {
    expect(roundTrip("> 引用一行\n")).toContain("> 引用一行");
    expect(roundTrip("---\n")).toContain("---");
  });

  it("单个换行不会塌成一个空格", () => {
    // 只读渲染给段落加了 white-space: pre-wrap，源码里的软换行本来是看得见的；
    // 编辑器若把它折成空格，用户的换行就凭空消失了。
    const out = roundTrip("第一行\n第二行\n");
    expect(out.replace(/\\\n/g, "\n")).toContain("第一行\n第二行");
    expect(out).not.toContain("第一行 第二行");
  });

  it("公式退化成普通文本但内容不丢", () => {
    // 已知缺口：没有接数学扩展，$$…$$ 在编辑期是纯文本，不会被解析成公式节点。
    const out = roundTrip("行内 $a^2$ 与块级：\n\n$$\nE = mc^2\n$$\n");
    expect(out).toContain("$a^2$");
    expect(out).toContain("E = mc^2");
  });
});

describe("表格编辑不会毁掉正文", () => {
  it("单元格里按回车是加一行，不是拆段落", () => {
    const element = document.createElement("div");
    const editor = editorFor("| 项 | 值 |\n| --- | --- |\n| a | 1 |\n", element);
    editor.commands.setTextSelection(4);
    const prevented = !editor.view.dom.dispatchEvent(
      new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true }),
    );

    expect(prevented).toBe(true);
    // 关键断言：序列化结果仍是管道表。多段落单元格会让它退化成 `[table]`，
    // 整张表的内容就此消失。
    const out = toMarkdown(editor);
    expect(out).toContain("| 项 | 值 |");
    expect(out).not.toContain("[table]");
    editor.destroy();
  });

  it("表格外的回车照常分段", () => {
    const element = document.createElement("div");
    const editor = editorFor("第一段", element);
    editor.commands.setTextSelection(3);
    editor.view.dom.dispatchEvent(
      new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true }),
    );
    expect(toMarkdown(editor).split("\n\n").length).toBeGreaterThan(1);
    editor.destroy();
  });
});

describe("受控同步", () => {
  beforeEach(() => {
    document.body.innerHTML = "";
  });

  it("外部换了内容就重建文档，且不回写", () => {
    const onChange = vi.fn();
    const view = render(<RichTextEditor value="旧的一篇" onChange={onChange} />);
    expect(document.querySelector(".notes-rt-content")?.textContent).toContain("旧的一篇");

    view.rerender(<RichTextEditor value="换了一篇" onChange={onChange} />);
    expect(document.querySelector(".notes-rt-content")?.textContent).toContain("换了一篇");
    // 回灌不是用户编辑，不能再触发一次保存。
    expect(onChange).not.toHaveBeenCalled();
  });

  it("外部值没变时不重建文档", () => {
    const onChange = vi.fn();
    const view = render(<RichTextEditor value="同一篇" onChange={onChange} />);
    const before = document.querySelector(".notes-rt-content");
    view.rerender(<RichTextEditor value="同一篇" onChange={onChange} />);
    expect(document.querySelector(".notes-rt-content")).toBe(before);
    expect(onChange).not.toHaveBeenCalled();
  });
});
