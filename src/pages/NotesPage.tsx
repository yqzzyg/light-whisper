import { useEffect, useRef, useState } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { ArrowLeft, Minus, X } from "lucide-react";
import { useTranslation } from "react-i18next";

import { hideMainWindow } from "@/api/tauri";
import NoteEditor from "@/components/notes/NoteEditor";
import NotesSidebar from "@/components/notes/NotesSidebar";
import TitleBar from "@/components/TitleBar";
import {
  closeNote,
  createFolder,
  createNote,
  deleteFolder,
  editNote,
  ensureNotesLoaded,
  flushNoteSave,
  loadMoreNotes,
  moveNote,
  openNote,
  removeNote,
  renameFolder,
  reorderFolders,
  setActiveFolder,
  setQuery,
  toggleFolder,
  useNotesStore,
} from "@/hooks/notesStore";

type View = "main" | "settings" | "history" | "upload" | "notes";

export default function NotesPage({
  onNavigate,
  animClass = "",
}: {
  onNavigate: (view: View) => void;
  animClass?: string;
}) {
  const { t } = useTranslation();
  const state = useNotesStore();
  const [search, setSearch] = useState(state.query);
  const searchTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  // 首次进入拉数据；离开页面前强制落库，防止防抖窗口里的改动丢掉。
  useEffect(() => {
    void ensureNotesLoaded();
    const flush = () => {
      void flushNoteSave();
    };
    window.addEventListener("pagehide", flush);
    window.addEventListener("beforeunload", flush);
    return () => {
      window.removeEventListener("pagehide", flush);
      window.removeEventListener("beforeunload", flush);
      flush();
    };
  }, []);

  useEffect(
    () => () => {
      if (searchTimer.current) clearTimeout(searchTimer.current);
    },
    [],
  );

  const handleSearch = (value: string) => {
    setSearch(value);
    if (searchTimer.current) clearTimeout(searchTimer.current);
    searchTimer.current = setTimeout(() => {
      void setQuery(value);
    }, 250);
  };

  const note = state.note;
  const untitled = t("notes.untitled");

  const handleDeleteNote = () => {
    if (!note) return;
    if (!window.confirm(t("notes.deleteNoteConfirm", { title: note.title.trim() || untitled }))) {
      return;
    }
    void removeNote(note.id);
  };

  const handleDeleteFolder = (id: number, name: string) => {
    if (!window.confirm(t("notes.deleteFolderConfirm", { name }))) return;
    void deleteFolder(id);
  };

  return (
    <div className={`page-root notes-page ${animClass}`}>
      <TitleBar
        title={t("notes.title")}
        leftAction={(
          <button
            className="icon-btn plain"
            aria-label={t("common.back")}
            onClick={() => onNavigate("main")}
          >
            <ArrowLeft size={14} />
          </button>
        )}
        rightActions={(
          <>
            <button
              aria-label={t("common.minimize")}
              className="icon-btn"
              onClick={() => getCurrentWindow().minimize()}
            >
              <Minus size={12} />
            </button>
            <button
              aria-label={t("common.close")}
              className="icon-btn"
              onClick={() => hideMainWindow()}
            >
              <X size={12} />
            </button>
          </>
        )}
      />

      <div className="page-content notes-content">
        <NotesSidebar
          folders={state.folders}
          notes={state.notes}
          total={state.total}
          hasMore={state.hasMore}
          loading={state.loadingNotes}
          query={search}
          activeFolderId={state.activeFolderId}
          collapsedFolderIds={state.collapsedFolderIds}
          selectedNoteId={state.selectedNoteId}
          onSearch={handleSearch}
          onToggleFolder={toggleFolder}
          onSelectFolder={setActiveFolder}
          onOpenNote={(id) => void openNote(id)}
          onCreateNote={(folderId) => void createNote(folderId)}
          onCreateFolder={async (name) => (await createFolder(name)) !== null}
          onRenameFolder={renameFolder}
          onDeleteFolder={handleDeleteFolder}
          onReorder={(ids) => void reorderFolders(ids)}
          onLoadMore={() => void loadMoreNotes()}
        />

        <NoteEditor
          note={note}
          folders={state.folders}
          loading={state.loadingNote}
          saving={state.saving}
          untitled={untitled}
          onEdit={editNote}
          onFlush={() => void flushNoteSave()}
          onMove={(folderId) => {
            if (note) void moveNote(note.id, folderId);
          }}
          onDelete={handleDeleteNote}
          onClose={() => closeNote()}
        />
      </div>
    </div>
  );
}
