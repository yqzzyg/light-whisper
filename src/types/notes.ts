export type NoteKind = "personal" | "meeting" | "import";

export interface NoteFolder {
  id: number;
  name: string;
  sortOrder: number;
  noteCount: number;
  createdAt: number;
  updatedAt: number;
}

export interface NoteListItem {
  id: number;
  folderId: number | null;
  title: string;
  kind: NoteKind;
  excerpt: string;
  durationSec: number | null;
  hasTranscript: boolean;
  hasSummary: boolean;
  hasAudio: boolean;
  createdAt: number;
  updatedAt: number;
}

export interface NoteDetail {
  id: number;
  folderId: number | null;
  title: string;
  content: string;
  transcript: string | null;
  summary: string | null;
  summaryModel: string | null;
  summaryGeneratedAt: number | null;
  summarySourceHash: string | null;
  kind: NoteKind;
  durationSec: number | null;
  historyId: number | null;
  audioFile: string | null;
  createdAt: number;
  updatedAt: number;
}

export interface NotePage {
  items: NoteListItem[];
  total: number;
  hasMore: boolean;
}

export interface NoteQuery {
  folderId?: number | null;
  /** 只看「未分类」；与 folderId 同时给出时以本项为准。 */
  unfiled?: boolean;
  query?: string;
  limit?: number;
  offset?: number;
}

/** 局部更新：只传要改的字段，缺省意味着「不改」。 */
export interface NotePatch {
  title?: string;
  content?: string;
}
