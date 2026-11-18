import { useSyncExternalStore } from "react";

import {
  createNote as apiCreateNote,
  createNoteFolder as apiCreateNoteFolder,
  createNoteFromHistory as apiCreateNoteFromHistory,
  deleteNote as apiDeleteNote,
  deleteNoteFolder as apiDeleteNoteFolder,
  getNote,
  listNoteFolders,
  listNotes,
  moveNote as apiMoveNote,
  renameNoteFolder as apiRenameNoteFolder,
  reorderNoteFolders as apiReorderNoteFolders,
  updateNote as apiUpdateNote,
} from "@/api/tauri";
import type { NoteDetail, NoteFolder, NoteListItem, NotePatch } from "@/types/notes";

const PAGE_SIZE = 200;
/// 自动保存防抖：太短会把每个字符都发一次 IPC，太长会在强制退出时丢字。
const SAVE_DEBOUNCE_MS = 600;

export interface NotesState {
  folders: NoteFolder[];
  foldersLoaded: boolean;
  /** 全量笔记（按更新时间倒序），由侧栏按目录分组渲染。 */
  notes: NoteListItem[];
  total: number;
  hasMore: boolean;
  query: string;
  /** 当前高亮的目录；新建笔记默认落到这里，`null` 表示「未分类」。 */
  activeFolderId: number | null;
  /** 收起的目录；默认全展开，所以存「收起」而不是「展开」。 */
  collapsedFolderIds: number[];
  selectedNoteId: number | null;
  note: NoteDetail | null;
  loadingFolders: boolean;
  loadingNotes: boolean;
  loadingNote: boolean;
  saving: boolean;
  error: string | null;
}

const initialState: NotesState = {
  folders: [],
  foldersLoaded: false,
  notes: [],
  total: 0,
  hasMore: false,
  query: "",
  activeFolderId: null,
  collapsedFolderIds: [],
  selectedNoteId: null,
  note: null,
  loadingFolders: false,
  loadingNotes: false,
  loadingNote: false,
  saving: false,
  error: null,
};

let state: NotesState = initialState;
const listeners = new Set<() => void>();

// 刷新列表与打开笔记都会异步返回，用递增令牌丢弃过期结果。
let notesToken = 0;
let noteToken = 0;
// 待落库的编辑内容：放在模块级，页面卸载后仍能被 flush。
let pendingPatch: NotePatch | null = null;
let pendingNoteId: number | null = null;
let saveTimer: ReturnType<typeof setTimeout> | null = null;

function emit(patch: Partial<NotesState>) {
  state = { ...state, ...patch };
  for (const listener of listeners) listener();
}

function messageOf(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

export function subscribeNotesStore(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

export function getNotesSnapshot(): NotesState {
  return state;
}

export function useNotesStore(): NotesState {
  return useSyncExternalStore(subscribeNotesStore, getNotesSnapshot, getNotesSnapshot);
}

function mergeNotes(current: NoteListItem[], incoming: NoteListItem[]): NoteListItem[] {
  const seen = new Set(current.map((item) => item.id));
  return [...current, ...incoming.filter((item) => !seen.has(item.id))];
}

function applyPatchToList(
  items: NoteListItem[],
  id: number,
  patch: NotePatch,
  updatedAt: number,
): NoteListItem[] {
  return items.map((item) =>
    item.id === id
      ? {
          ...item,
          title: patch.title ?? item.title,
          excerpt: (patch.content ?? item.excerpt).slice(0, 140),
          updatedAt,
        }
      : item,
  );
}

function expandFolder(collapsed: number[], folderId: number | null): number[] {
  if (folderId === null || !collapsed.includes(folderId)) return collapsed;
  return collapsed.filter((id) => id !== folderId);
}

// ---------------------------------------------------------------------------
// 读取
// ---------------------------------------------------------------------------

export async function refreshFolders(): Promise<void> {
  emit({ loadingFolders: true });
  try {
    const folders = await listNoteFolders();
    emit({ folders, foldersLoaded: true, loadingFolders: false });
  } catch (error) {
    emit({ loadingFolders: false, error: messageOf(error) });
  }
}

export async function refreshNotes(reset = true): Promise<void> {
  const token = ++notesToken;
  const offset = reset ? 0 : state.notes.length;
  emit({ loadingNotes: true });
  try {
    const page = await listNotes({
      query: state.query,
      limit: PAGE_SIZE,
      offset,
    });
    if (token !== notesToken) return;
    emit({
      notes: reset ? page.items : mergeNotes(state.notes, page.items),
      total: page.total,
      hasMore: page.hasMore,
      loadingNotes: false,
      error: null,
    });
  } catch (error) {
    if (token !== notesToken) return;
    emit({ loadingNotes: false, error: messageOf(error) });
  }
}

export async function loadMoreNotes(): Promise<void> {
  if (state.loadingNotes || !state.hasMore) return;
  await refreshNotes(false);
}

/** 进入页面时调用：目录只在首次拉取，列表每次都刷，避免别的入口（历史/上传转笔记）新增的笔记不可见。 */
export async function ensureNotesLoaded(): Promise<void> {
  if (!state.foldersLoaded) await refreshFolders();
  await refreshNotes(true);
}

/** 目录不需要查询，点一下只是换高亮与「新建笔记」的落点；展开状态单独管。 */
export function setActiveFolder(folderId: number | null): void {
  emit({ activeFolderId: folderId, collapsedFolderIds: expandFolder(state.collapsedFolderIds, folderId) });
}

export function toggleFolder(folderId: number): void {
  const collapsed = state.collapsedFolderIds.includes(folderId)
    ? state.collapsedFolderIds.filter((id) => id !== folderId)
    : [...state.collapsedFolderIds, folderId];
  emit({ collapsedFolderIds: collapsed });
}

export async function setQuery(query: string): Promise<void> {
  emit({ query, collapsedFolderIds: [] });
  await refreshNotes(true);
}

export async function openNote(id: number): Promise<void> {
  await flushNoteSave();
  const token = ++noteToken;
  emit({ selectedNoteId: id, loadingNote: true });
  try {
    const note = await getNote(id);
    if (token !== noteToken) return;
    emit({
      note,
      selectedNoteId: note ? note.id : null,
      activeFolderId: note ? note.folderId : state.activeFolderId,
      loadingNote: false,
      error: null,
    });
  } catch (error) {
    if (token !== noteToken) return;
    emit({ loadingNote: false, error: messageOf(error) });
  }
}

export function closeNote(): void {
  void flushNoteSave();
  noteToken += 1;
  emit({ selectedNoteId: null, note: null, loadingNote: false });
}

/**
 * 重新读取当前打开的笔记，不显示加载骨架。
 *
 * 用于后端改了笔记而前端不知情的场景（停止录音后回填 audioFile/duration、
 * 转写完成写回转录）。先 flush 待保存的编辑，否则这次重读会把刚敲的字覆盖掉。
 */
export async function reloadOpenNote(): Promise<void> {
  const id = state.selectedNoteId;
  if (id == null) return;
  await flushNoteSave();
  const token = ++noteToken;
  try {
    const note = await getNote(id);
    if (token !== noteToken || state.selectedNoteId !== id) return;
    emit({ note });
  } catch (error) {
    // 重读失败不影响当前展示，下次打开会重新拉。
    console.warn("刷新笔记失败:", messageOf(error));
  }
}

// ---------------------------------------------------------------------------
// 写入
// ---------------------------------------------------------------------------

/** 编辑标题或正文：先立刻上屏，再防抖落库。 */
export function editNote(patch: NotePatch): void {
  const note = state.note;
  if (!note) return;
  emit({ note: { ...note, ...patch }, saving: true });
  pendingPatch = { ...(pendingPatch ?? {}), ...patch };
  pendingNoteId = note.id;
  if (saveTimer !== null) clearTimeout(saveTimer);
  saveTimer = setTimeout(() => {
    saveTimer = null;
    void flushNoteSave();
  }, SAVE_DEBOUNCE_MS);
}

/** 立即落库。切笔记、切页、退出前都调它，避免防抖窗口内丢字。 */
export async function flushNoteSave(): Promise<void> {
  if (saveTimer !== null) {
    clearTimeout(saveTimer);
    saveTimer = null;
  }
  const patch = pendingPatch;
  const noteId = pendingNoteId;
  if (!patch || noteId === null) return;
  pendingPatch = null;
  pendingNoteId = null;
  try {
    const saved = await apiUpdateNote(noteId, patch);
    const updatedAt = saved?.updatedAt ?? Date.now();
    emit({
      saving: false,
      error: null,
      notes: applyPatchToList(state.notes, noteId, patch, updatedAt),
      note: state.note && state.note.id === noteId ? { ...state.note, updatedAt } : state.note,
    });
  } catch (error) {
    // 失败时把这次改动放回队列，下一次 flush 重试，而不是静默丢掉。
    pendingPatch = { ...patch, ...(pendingPatch ?? {}) };
    pendingNoteId = noteId;
    emit({ saving: false, error: messageOf(error) });
  }
}

export async function createFolder(name: string): Promise<NoteFolder | null> {
  try {
    const folder = await apiCreateNoteFolder(name);
    await refreshFolders();
    emit({ activeFolderId: folder.id });
    return folder;
  } catch (error) {
    emit({ error: messageOf(error) });
    return null;
  }
}

export async function renameFolder(id: number, name: string): Promise<boolean> {
  try {
    await apiRenameNoteFolder(id, name);
    await refreshFolders();
    return true;
  } catch (error) {
    emit({ error: messageOf(error) });
    return false;
  }
}

export async function deleteFolder(id: number): Promise<void> {
  try {
    await apiDeleteNoteFolder(id);
    emit({
      activeFolderId: state.activeFolderId === id ? null : state.activeFolderId,
      collapsedFolderIds: state.collapsedFolderIds.filter((item) => item !== id),
    });
    await Promise.all([refreshFolders(), refreshNotes(true)]);
  } catch (error) {
    emit({ error: messageOf(error) });
  }
}

export async function reorderFolders(ids: number[]): Promise<void> {
  try {
    const folders = await apiReorderNoteFolders(ids);
    emit({ folders });
  } catch (error) {
    emit({ error: messageOf(error) });
  }
}

export async function createNote(folderId: number | null): Promise<void> {
  await flushNoteSave();
  try {
    const note = await apiCreateNote(folderId, "", "personal");
    emit({ activeFolderId: folderId, collapsedFolderIds: expandFolder(state.collapsedFolderIds, folderId) });
    await Promise.all([refreshFolders(), refreshNotes(true)]);
    emit({ selectedNoteId: note.id, note, loadingNote: false, error: null });
  } catch (error) {
    emit({ error: messageOf(error) });
  }
}

/**
 * 历史 / 上传记录存为笔记：转录随笔记走，音频仍归历史记录。
 *
 * 直接切到这条笔记：用户点「存为笔记」的意图就是去看它。这里不吞异常，
 * 调用方需要把后端文案（「这条记录没有转写文本」）原样告诉用户。
 */
export async function createNoteFromHistory(historyId: number): Promise<NoteDetail> {
  await flushNoteSave();
  const note = await apiCreateNoteFromHistory(historyId);
  await Promise.all([refreshFolders(), refreshNotes(true)]);
  emit({
    selectedNoteId: note.id,
    note,
    activeFolderId: note.folderId,
    collapsedFolderIds: expandFolder(state.collapsedFolderIds, note.folderId),
    loadingNote: false,
    error: null,
  });
  return note;
}

export async function moveNote(id: number, folderId: number | null): Promise<void> {
  try {
    const moved = await apiMoveNote(id, folderId);
    if (moved && state.note && state.note.id === id) {
      emit({
        note: { ...state.note, folderId: moved.folderId },
        activeFolderId: moved.folderId,
        collapsedFolderIds: expandFolder(state.collapsedFolderIds, moved.folderId),
      });
    }
    await Promise.all([refreshFolders(), refreshNotes(true)]);
  } catch (error) {
    emit({ error: messageOf(error) });
  }
}

export async function removeNote(id: number): Promise<void> {
  try {
    await apiDeleteNote(id);
    if (state.selectedNoteId === id) {
      noteToken += 1;
      emit({ selectedNoteId: null, note: null });
    }
    await Promise.all([refreshFolders(), refreshNotes(true)]);
  } catch (error) {
    emit({ error: messageOf(error) });
  }
}

/** 只给测试用：把模块级状态恢复到初始值。 */
export function resetNotesStoreForTests(): void {
  if (saveTimer !== null) {
    clearTimeout(saveTimer);
    saveTimer = null;
  }
  pendingPatch = null;
  pendingNoteId = null;
  notesToken += 1;
  noteToken += 1;
  state = initialState;
  for (const listener of listeners) listener();
}
