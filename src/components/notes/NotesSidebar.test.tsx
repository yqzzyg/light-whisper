import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

vi.mock("react-i18next", () => ({
  useTranslation: () => ({
    i18n: { language: "zh" },
    t: (key: string) => key,
  }),
}));

import NotesSidebar from "@/components/notes/NotesSidebar";
import type { NoteFolder, NoteListItem } from "@/types/notes";

const meeting: NoteFolder = {
  id: 3,
  name: "会议",
  sortOrder: 0,
  noteCount: 2,
  createdAt: 1,
  updatedAt: 1,
};
const project: NoteFolder = {
  id: 4,
  name: "项目",
  sortOrder: 1,
  noteCount: 0,
  createdAt: 1,
  updatedAt: 1,
};

function note(id: number, title: string, folderId: number | null): NoteListItem {
  return {
    id,
    folderId,
    title,
    kind: "personal",
    excerpt: "",
    durationSec: null,
    hasTranscript: false,
    hasSummary: false,
    hasAudio: false,
    createdAt: 1,
    updatedAt: 1,
  };
}

function setup(overrides: Partial<React.ComponentProps<typeof NotesSidebar>> = {}) {
  const props = {
    folders: [meeting, project],
    notes: [note(1, "周会记录", 3), note(2, "随手记", null)],
    total: 2,
    hasMore: false,
    loading: false,
    query: "",
    activeFolderId: 3 as number | null,
    collapsedFolderIds: [] as number[],
    selectedNoteId: 1 as number | null,
    onSearch: vi.fn(),
    onToggleFolder: vi.fn(),
    onSelectFolder: vi.fn(),
    onOpenNote: vi.fn(),
    onCreateNote: vi.fn(),
    onCreateFolder: vi.fn().mockResolvedValue(true),
    onRenameFolder: vi.fn().mockResolvedValue(true),
    onDeleteFolder: vi.fn(),
    onReorder: vi.fn(),
    onLoadMore: vi.fn(),
    ...overrides,
  };
  render(<NotesSidebar {...props} />);
  return props;
}

describe("NotesSidebar", () => {
  it("lists notes directly under their folder instead of a separate column", () => {
    setup();
    expect(screen.getByText("会议")).toBeTruthy();
    expect(screen.getByText("周会记录")).toBeTruthy();
    // 未分类单独一组，组内笔记同样直接展示
    expect(screen.getByText("notes.unfiled")).toBeTruthy();
    expect(screen.getByText("随手记")).toBeTruthy();
  });

  it("opens a note when its title is clicked", () => {
    const props = setup();
    fireEvent.click(screen.getByText("周会记录"));
    expect(props.onOpenNote).toHaveBeenCalledWith(1);
  });

  it("hides the notes of a collapsed folder", () => {
    setup({ collapsedFolderIds: [3] });
    expect(screen.queryByText("周会记录")).toBeNull();
    expect(screen.getByText("随手记")).toBeTruthy();
  });

  it("toggles a folder from the chevron and selects it from the name", () => {
    const props = setup();
    fireEvent.click(screen.getAllByLabelText("notes.collapse")[0]);
    expect(props.onToggleFolder).toHaveBeenCalledWith(3);

    fireEvent.click(screen.getByText("项目"));
    expect(props.onSelectFolder).toHaveBeenCalledWith(4);
  });

  it("creates a note inside the folder from the row action", () => {
    const props = setup();
    fireEvent.click(screen.getAllByLabelText("notes.newNote")[1]);
    expect(props.onCreateNote).toHaveBeenCalledWith(3);
  });

  it("creates a folder from the inline input and closes it", async () => {
    const props = setup();
    fireEvent.click(screen.getByLabelText("notes.newFolder"));

    const input = screen.getByPlaceholderText("notes.folderNamePlaceholder");
    fireEvent.change(input, { target: { value: "  周会  " } });
    fireEvent.keyDown(input, { key: "Enter" });

    expect(props.onCreateFolder).toHaveBeenCalledWith("周会");
    await waitFor(() =>
      expect(screen.queryByPlaceholderText("notes.folderNamePlaceholder")).toBeNull(),
    );
  });

  it("renames a folder by double clicking its name", () => {
    const props = setup();
    fireEvent.doubleClick(screen.getByText("会议"));

    const input = screen.getByDisplayValue("会议");
    fireEvent.change(input, { target: { value: "会议纪要" } });
    fireEvent.keyDown(input, { key: "Enter" });

    expect(props.onRenameFolder).toHaveBeenCalledWith(3, "会议纪要");
  });

  it("deletes with both id and name so the confirm text can mention it", () => {
    const props = setup();
    fireEvent.click(screen.getAllByLabelText("common.delete")[0]);
    expect(props.onDeleteFolder).toHaveBeenCalledWith(3, "会议");
  });

  it("reorders by swapping with the neighbour", () => {
    const props = setup();
    fireEvent.click(screen.getAllByLabelText("notes.moveDown")[0]);
    expect(props.onReorder).toHaveBeenCalledWith([4, 3]);
  });

  it("marks the active folder and the open note", () => {
    setup();
    const folderRow = screen.getByText("会议").closest(".notes-folder-row") as HTMLElement | null;
    expect(folderRow?.dataset.active).toBe("true");
    expect(screen.getByText("周会记录").closest("button")?.dataset.active).toBe("true");
  });

  it("forwards search and hides folder counts while searching", () => {
    const props = setup({ query: "周会" });
    const input = screen.getByPlaceholderText("notes.searchPlaceholder");
    fireEvent.change(input, { target: { value: "周会纪要" } });
    expect(props.onSearch).toHaveBeenCalledWith("周会纪要");
    expect(screen.queryByText("2")).toBeNull();
  });

  it("offers a load-more entry only when there are more notes", () => {
    const props = setup({ hasMore: true, total: 400 });
    fireEvent.click(screen.getByText("notes.loadMore"));
    expect(props.onLoadMore).toHaveBeenCalled();
  });
});
