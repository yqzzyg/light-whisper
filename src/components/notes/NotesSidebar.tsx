import { useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  ChevronDown,
  ChevronRight,
  ChevronUp,
  FileText,
  Folder,
  FolderPlus,
  Inbox,
  Pencil,
  Plus,
  Search,
  Trash2,
} from "lucide-react";

import type { NoteFolder, NoteListItem } from "@/types/notes";

export interface NotesSidebarProps {
  folders: NoteFolder[];
  notes: NoteListItem[];
  total: number;
  hasMore: boolean;
  loading: boolean;
  query: string;
  activeFolderId: number | null;
  collapsedFolderIds: number[];
  selectedNoteId: number | null;
  onSearch: (value: string) => void;
  onToggleFolder: (id: number) => void;
  onSelectFolder: (id: number | null) => void;
  onOpenNote: (id: number) => void;
  onCreateNote: (folderId: number | null) => void;
  onCreateFolder: (name: string) => Promise<boolean>;
  onRenameFolder: (id: number, name: string) => Promise<boolean>;
  onDeleteFolder: (id: number, name: string) => void;
  onReorder: (ids: number[]) => void;
  onLoadMore: () => void;
}

/**
 * 两栏布局的左栏：目录树，笔记直接挂在目录下面。
 *
 * 目录只有一层，所以「展开」只控制笔记列表显隐，不做嵌套。
 * 重命名与新建目录都用行内输入框，避免为一段字符串再引一个弹窗组件。
 */
export default function NotesSidebar({
  folders,
  notes,
  total,
  hasMore,
  loading,
  query,
  activeFolderId,
  collapsedFolderIds,
  selectedNoteId,
  onSearch,
  onToggleFolder,
  onSelectFolder,
  onOpenNote,
  onCreateNote,
  onCreateFolder,
  onRenameFolder,
  onDeleteFolder,
  onReorder,
  onLoadMore,
}: NotesSidebarProps) {
  const { t } = useTranslation();
  const [creating, setCreating] = useState(false);
  const [draftName, setDraftName] = useState("");
  const [editingId, setEditingId] = useState<number | null>(null);
  const [editingName, setEditingName] = useState("");

  // 笔记已按更新时间倒序，分组时保持原顺序即可。
  const grouped = useMemo(() => {
    const byFolder = new Map<number, NoteListItem[]>();
    const unfiled: NoteListItem[] = [];
    for (const note of notes) {
      if (note.folderId == null) {
        unfiled.push(note);
        continue;
      }
      const bucket = byFolder.get(note.folderId);
      if (bucket) bucket.push(note);
      else byFolder.set(note.folderId, [note]);
    }
    return { byFolder, unfiled };
  }, [notes]);

  const submitCreate = async () => {
    const name = draftName.trim();
    if (!name) {
      setCreating(false);
      setDraftName("");
      return;
    }
    const created = await onCreateFolder(name);
    if (created) {
      setCreating(false);
      setDraftName("");
    }
  };

  const submitRename = async (id: number) => {
    const name = editingName.trim();
    setEditingId(null);
    if (!name) return;
    await onRenameFolder(id, name);
  };

  const move = (index: number, delta: number) => {
    const target = index + delta;
    if (target < 0 || target >= folders.length) return;
    const ids = folders.map((folder) => folder.id);
    [ids[index], ids[target]] = [ids[target], ids[index]];
    onReorder(ids);
  };

  const renderNotes = (items: NoteListItem[]) =>
    items.map((item) => (
      <button
        key={item.id}
        type="button"
        className="notes-note-row"
        data-active={selectedNoteId === item.id}
        title={item.title.trim() || t("notes.untitled")}
        onClick={() => onOpenNote(item.id)}
      >
        <FileText size={13} aria-hidden="true" />
        <span className="notes-note-title">{item.title.trim() || t("notes.untitled")}</span>
        {item.hasAudio && <span className="notes-note-dot" title={t("notes.hasAudio")} />}
      </button>
    ));

  const renderFolder = (folder: NoteFolder, index: number) => {
    const collapsed = collapsedFolderIds.includes(folder.id);
    const items = grouped.byFolder.get(folder.id) ?? [];

    return (
      <div key={folder.id} className="notes-group">
        <div className="notes-folder-row" data-active={activeFolderId === folder.id}>
          <button
            type="button"
            className="notes-folder-toggle"
            aria-label={collapsed ? t("notes.expand") : t("notes.collapse")}
            onClick={() => onToggleFolder(folder.id)}
          >
            {collapsed ? <ChevronRight size={13} /> : <ChevronDown size={13} />}
          </button>

          {editingId === folder.id ? (
            <input
              className="notes-folder-input"
              value={editingName}
              autoFocus
              onChange={(event) => setEditingName(event.target.value)}
              onBlur={() => void submitRename(folder.id)}
              onKeyDown={(event) => {
                if (event.key === "Enter") void submitRename(folder.id);
                if (event.key === "Escape") setEditingId(null);
              }}
            />
          ) : (
            <button
              type="button"
              className="notes-folder-main"
              onClick={() => {
                onSelectFolder(folder.id);
                if (collapsed) onToggleFolder(folder.id);
              }}
              onDoubleClick={() => {
                setEditingId(folder.id);
                setEditingName(folder.name);
              }}
            >
              <Folder size={14} aria-hidden="true" />
              <span className="notes-folder-name" title={folder.name}>
                {folder.name}
              </span>
              {!query && <span className="notes-folder-count">{folder.noteCount}</span>}
            </button>
          )}

          <div className="notes-folder-actions">
            <button
              type="button"
              className="icon-btn plain"
              aria-label={t("notes.newNote")}
              title={t("notes.newNote")}
              onClick={() => onCreateNote(folder.id)}
            >
              <Plus size={13} />
            </button>
            <button
              type="button"
              className="icon-btn plain"
              aria-label={t("notes.moveUp")}
              title={t("notes.moveUp")}
              disabled={index === 0}
              onClick={() => move(index, -1)}
            >
              <ChevronUp size={13} />
            </button>
            <button
              type="button"
              className="icon-btn plain"
              aria-label={t("notes.moveDown")}
              title={t("notes.moveDown")}
              disabled={index === folders.length - 1}
              onClick={() => move(index, 1)}
            >
              <ChevronDown size={13} />
            </button>
            <button
              type="button"
              className="icon-btn plain"
              aria-label={t("notes.rename")}
              title={t("notes.rename")}
              onClick={() => {
                setEditingId(folder.id);
                setEditingName(folder.name);
              }}
            >
              <Pencil size={13} />
            </button>
            <button
              type="button"
              className="icon-btn plain"
              aria-label={t("common.delete")}
              title={t("common.delete")}
              onClick={() => onDeleteFolder(folder.id, folder.name)}
            >
              <Trash2 size={13} />
            </button>
          </div>
        </div>

        {!collapsed && items.length > 0 && (
          <div className="notes-folder-notes">{renderNotes(items)}</div>
        )}
      </div>
    );
  };

  const unfiledCount = grouped.unfiled.length;
  const showUnfiled = unfiledCount > 0;
  const nothing = folders.length === 0 && notes.length === 0;

  return (
    <aside className="notes-sidebar">
      <div className="notes-sidebar-head">
        <span className="notes-sidebar-title">{t("notes.folders")}</span>
        <div className="notes-sidebar-tools">
          <button
            type="button"
            className="icon-btn plain"
            aria-label={t("notes.newFolder")}
            title={t("notes.newFolder")}
            onClick={() => {
              setCreating(true);
              setDraftName("");
            }}
          >
            <FolderPlus size={15} />
          </button>
          <button
            type="button"
            className="icon-btn plain"
            aria-label={t("notes.newNote")}
            title={t("notes.newNote")}
            onClick={() => onCreateNote(activeFolderId)}
          >
            <Plus size={16} />
          </button>
        </div>
      </div>

      <div className="notes-search">
        <Search size={13} aria-hidden="true" />
        <input
          value={query}
          placeholder={t("notes.searchPlaceholder")}
          onChange={(event) => onSearch(event.target.value)}
        />
      </div>

      <div className="notes-sidebar-body">
        {folders.map(renderFolder)}

        {showUnfiled && (
          <div className="notes-group">
            <div className="notes-folder-row" data-active={activeFolderId === null}>
              <span className="notes-folder-toggle notes-folder-toggle-static" aria-hidden="true" />
              <button
                type="button"
                className="notes-folder-main"
                onClick={() => onSelectFolder(null)}
              >
                <Inbox size={14} aria-hidden="true" />
                <span className="notes-folder-name">{t("notes.unfiled")}</span>
                {!query && !hasMore && <span className="notes-folder-count">{unfiledCount}</span>}
              </button>
            </div>
            <div className="notes-folder-notes">{renderNotes(grouped.unfiled)}</div>
          </div>
        )}

        {creating && (
          <input
            className="notes-folder-input notes-folder-input-create"
            value={draftName}
            autoFocus
            placeholder={t("notes.folderNamePlaceholder")}
            onChange={(event) => setDraftName(event.target.value)}
            onBlur={() => void submitCreate()}
            onKeyDown={(event) => {
              if (event.key === "Enter") void submitCreate();
              if (event.key === "Escape") {
                setCreating(false);
                setDraftName("");
              }
            }}
          />
        )}

        {hasMore && (
          <button
            type="button"
            className="notes-load-more"
            onClick={onLoadMore}
            disabled={loading}
          >
            {t("notes.loadMore")}
          </button>
        )}

        {nothing && !loading && (
          <div className="notes-sidebar-empty">
            <p>{query ? t("notes.emptySearch") : t("notes.emptyNotes")}</p>
            {!query && (
              <button
                type="button"
                className="notes-empty-action"
                onClick={() => onCreateNote(null)}
              >
                {t("notes.newNote")}
              </button>
            )}
          </div>
        )}

        {!nothing && notes.length === 0 && !loading && (
          <div className="notes-sidebar-empty">
            <p>{query ? t("notes.emptySearch") : t("notes.emptyNotes")}</p>
          </div>
        )}
      </div>

      {total > 0 && (
        <div className="notes-sidebar-foot">{t("notes.total", { count: total })}</div>
      )}
    </aside>
  );
}
