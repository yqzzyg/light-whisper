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
  /** 转录或正文在摘要生成后变过；由后端比对指纹得出。 */
  summaryStale: boolean;
  kind: NoteKind;
  durationSec: number | null;
  historyId: number | null;
  createdAt: number;
  updatedAt: number;
}

/** 一条笔记里的一段录音：一次 start/stop 就是一段，独立文件、独立转录。 */
export interface NoteRecording {
  id: number;
  noteId: number;
  audioFile: string;
  durationSec: number | null;
  transcript: string | null;
  historyId: number | null;
  sortOrder: number;
  createdAt: number;
}

/**
 * 一段录音里的一个说话人片段。
 *
 * `recordingId` 必须保留：同一篇笔记的多段录音各自从 0 开始计时，混成一个时间轴
 * 会重叠。`overlap` 表示这段里两个人同时说话，界面只用它加个提示。
 */
export interface NoteSpeakerSegment {
  recordingId: number;
  /** 从 0 开始的说话人序号，同一段录音内一致；不同录音之间没有关系。 */
  speakerId: number;
  startMs: number;
  endMs: number;
  text: string;
  overlap: boolean;
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

/**
 * 会议录音状态，与后端 `NoteRecordingSnapshot` 一一对应。
 *
 * 录音期间后端每 500ms 推一次 `note-recording-state`；停止/取消时最后一个
 * 事件里 `active` 为 false，并带上 `audioFile`/`durationSec`。
 */
export interface NoteRecordingSnapshot {
  active: boolean;
  sessionId: number | null;
  noteId: number | null;
  elapsedSec: number;
  /** 0.0–1.0 的峰值电平，仅用于画电平条。 */
  level: number;
  audioFile: string | null;
  durationSec: number | null;
  error: string | null;
}
