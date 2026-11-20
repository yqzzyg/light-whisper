import { useEffect, useRef, useState } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { ArrowLeft, Minus, X } from "lucide-react";
import { toast } from "sonner";
import { useTranslation } from "react-i18next";

import { hideMainWindow, revealNoteAudio } from "@/api/tauri";
import NoteEditor from "@/components/notes/NoteEditor";
import NotesSidebar from "@/components/notes/NotesSidebar";
import RecordingBar from "@/components/notes/RecordingBar";
import TitleBar from "@/components/TitleBar";
import { ensureNoteRecordingReady, startRecording, useNoteRecording } from "@/hooks/noteRecordingStore";
import {
  transcribeNote,
  transcribeSegment,
  useNoteTranscription,
} from "@/hooks/noteTranscriptionStore";
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
  removeNoteRecording,
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
  const recording = useNoteRecording();
  const transcription = useNoteTranscription();
  const [search, setSearch] = useState(state.query);
  const searchTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  // 首次进入拉数据；离开页面前强制落库，防止防抖窗口里的改动丢掉。
  useEffect(() => {
    void ensureNotesLoaded();
    // 录音由后端驱动，订阅挂在模块上：离开笔记页再回来仍能看到进行中的录音。
    void ensureNoteRecordingReady();
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

  const handleStartRecording = async () => {
    if (!note || recording.active) return;
    const error = await startRecording(note.id);
    if (error) toast.error(t("notes.recordFailed", { message: error }));
  };

  /**
   * 转写所有未转写的段。命令可能跑几分钟，但用户看到的是「已用时 + 预估」，
   * 走开了也会照常完成（状态在模块级 store 里，不随组件卸载丢失）。
   */
  const handleTranscribe = async () => {
    if (!note || transcription.running) return;
    const error = await transcribeNote(note.id);
    if (error) {
      toast.error(t("notes.transcribeFailed", { message: error }));
      return;
    }
    toast.success(t("notes.transcribeDone"));
  };

  /** 整篇重新转写：覆盖现有转录（换引擎重转走这条路），所以先确认一次。 */
  const handleRetranscribe = async () => {
    if (!note || transcription.running) return;
    if (!window.confirm(t("notes.retranscribeAllConfirm"))) return;
    const error = await transcribeNote(note.id, true);
    if (error) {
      toast.error(t("notes.transcribeFailed", { message: error }));
      return;
    }
    toast.success(t("notes.retranscribeAllDone"));
  };

  /** 只重转一段：已转写的段也允许重来，挑出识别不佳的那一段。 */
  const handleTranscribeSegment = async (recordingId: number) => {
    if (!note || transcription.running) return;
    const error = await transcribeSegment(note.id, recordingId);
    if (error) {
      toast.error(t("notes.transcribeFailed", { message: error }));
      return;
    }
    toast.success(t("notes.segmentTranscribeDone"));
  };

  const handleRevealRecording = async (id: number) => {
    try {
      await revealNoteAudio(id);
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      toast.error(t("notes.revealAudioFailed", { message }));
    }
  };

  const handleDeleteRecording = (id: number) => {
    if (!window.confirm(t("notes.deleteRecordingConfirm"))) return;
    void removeNoteRecording(id).then((removed) => {
      if (removed) toast.success(t("notes.recordingDeleted"));
    });
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

        <div className="notes-main">
          <RecordingBar />
          <NoteEditor
            note={note}
            folders={state.folders}
            loading={state.loadingNote}
            saving={state.saving}
            untitled={untitled}
            recording={recording}
            recordings={state.recordings}
            transcription={transcription}
            onStartRecording={() => void handleStartRecording()}
            onTranscribe={() => void handleTranscribe()}
            onRetranscribe={() => void handleRetranscribe()}
            onTranscribeSegment={(id) => void handleTranscribeSegment(id)}
            onRevealRecording={(id) => void handleRevealRecording(id)}
            onDeleteRecording={handleDeleteRecording}
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
    </div>
  );
}
