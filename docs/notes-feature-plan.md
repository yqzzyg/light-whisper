# 笔记功能 — 方案调研与工作清单

> 状态：方案已定，待实现 · 最后更新：2026-11-18
> 目标：左侧栏新增「笔记」，支持建立笔记目录、在目录下创建笔记；最终能在会议时录音并据此生成笔记。
> 参考实现：`D:\MyData\Code\CodeWorkSpace\StudyProject\StudyAi\whisper\openwhispr`（桌面端 + 移动端）。

## 1. 需求边界

| 项 | 一期决策 | 备注 |
|---|---|---|
| 入口 | 左侧栏「笔记」导航项，独立页面（两栏：目录树 + 编辑器） | 参考截图布局 |
| 目录 | **单层目录**（无嵌套），可增删改名、可排序 | openwhispr 也是单层；嵌套留 `parent_id` 扩展位 |
| 笔记归属 | 笔记属于某个目录，也可「未归类」 | 目录删除时笔记回到未归类，不级联删除 |
| 笔记内容 | 三块：AI 摘要 / 我的手写笔记 / 转录原文 | 对应截图的两个 tab + 摘要 tab |
| 编辑器 | **Markdown 源码 + 预览切换**（`react-markdown` 已依赖） | 不引入 tiptap |
| 会议录音 | 在笔记里开始/结束录音，录音结果落到该笔记 | 复用现有采集与 ASR 链路 |
| 录音方式 | **默认先录后转**：录音期间只采集落盘，不跑流式 ASR | 实时转录要全程跑 ASR，长会议耗电明显；见 §3.5 |
| 转写时机 | 停止后**只存录音文件**，用户点「转文字」才整段转写（复用上传转录接口） | 手动触发；本地引擎约 19× 实时 |
| 录音来源 | **仅麦克风**，不做系统声环回 | 线上会议录不到对方，列为已知限制 |
| AI 生成 | 基于转录（+手写笔记）生成结构化摘要，长文本分块 | 复用现有 LLM 通道 |
| 生成时机 | **只在用户点「生成笔记」时触发** | 不做停止录音后自动生成 |
| 不做（一期） | 会议中实时上屏、说话人分离、时间戳、系统声、云端同步、分享协作、目录嵌套、模板市场 | 见 §6 |

## 2. 参考项目调研结论

### 2.1 规模（说明不能照搬）

| 部分 | 规模 |
|---|---|
| `src/components/notes/*` | 43 个组件 / 18.7k 行（`SpacesTree` 2155、`NoteEditor` 1703、`PersonalNotesView` 1170、`ShareNoteDialog` 1226） |
| `src/helpers/database.js` | 10.9k 行（含多账号、团队空间、云同步、分享） |
| `src/helpers/ipcHandlers.js` | 15.3k 行（135+ 个笔记相关 IPC） |

openwhispr 是个协作化产品（空间/团队/邀请/分享/云同步/冲突解决）。我们要的是一台本地单人笔记工具，**只借数据结构与交互骨架，不借同步与权限**。

### 2.2 数据模型（值得抄的部分）

桌面端 `notes` 表（`database.js:879` 起，经过 30+ 次 `ALTER TABLE`）核心列：

```
id, title, content, note_type, source_file, audio_duration_seconds,
transcript, participants, folder_id, enhanced_content,
enhancement_prompt, enhancement_template_id, enhanced_at_content_hash,
created_at, updated_at
```

- `content` = 用户手写的笔记；`transcript` = 录音转录；`enhanced_content` = AI 生成的摘要。**三块分开存**，正是截图三个 tab 的来源。
- `enhanced_at_content_hash` = 生成摘要时的素材指纹，用来判断「转录变了、摘要过期了」。
- `folders` 表：`id, name(UNIQUE), is_default, sort_order`，**扁平无 parent_id**，首次启动种子三个目录（Personal / Meetings / Videos）。
- `transcript` 存的是 **JSON 数组字符串**（`parseTranscriptSegments.ts`），元素 `{text, source: mic|system, timestamp, speaker, speakerName...}`；没有分段时退化为纯文本单段。

### 2.3 关键流程

**新建笔记**（`useCreateNote.ts`）：写一行 `saveNote(未命名, "", "personal", ..., folderId, spaceId)` → 展开目录树 → 选中新笔记 → 若允许录音且当前没在录音，直接开始录音。

**编辑器**（`NoteEditor.tsx:1304-1333`）：`viewMode` 三态 `transcript | raw | enhanced`，分段按钮在标题下方；`enhanced` tab 只在有摘要时出现；每个 tab 用独立 `RichTextEditor key`，防止撤销栈跨 tab 互相污染（这条经验值得直接照抄）。

**录音绑定笔记**：`startRecording({noteId, noteTitle, folderId, seedSegments, ...})`，录音过程中段落累积在内存 store，停止时整体写回 `notes.transcript`；页面切走不中断（`useMeetingNoteSession` 只做「离开笔记页就释放会话占用」，不停止录音）。

**生成摘要**（`actionProcessingStore.ts`）：map-reduce。

| 常量 | 值 | 含义 |
|---|---|---|
| `MAX_PART_TOKENS` | 12288 | 单块上限（约 1 小时语音），再大模型会丢细节 |
| `PART_NOTES_MAX_TOKENS` | 2048 | 每块产出的笔记预算 |
| `CHUNK_FILL_FRACTION` | 0.85 | 按可用窗口的 85% 装块 |
| `MIN_CHUNK_BUDGET_TOKENS` | 1024 | 低于此值直接拒绝，不硬切 |
| `CONTEXT_RESERVE_TOKENS` | 512 | 系统提示预留 |

流程：转录按段落切块 → 每块用「只写这一段、不要标题、不要总结全文」的提示生成片段 → 把所有片段按 `## Notes from part N of M` 拼起来 → 用 merge 提示做最终一遍，产出正式摘要（Summary / Discussion / Decisions / Action Items）。

进度以「第 2/5 段」形式回传，可取消。这套分块策略是本项目长会议最需要的部分。

### 2.4 明确不抄的部分

- `spaces` / `workspaces` / 团队成员 / 邀请 / 角色权限（`NotePermissions`、`SpaceMembersPanel`）。
- 云同步与冲突解决（`SyncService`、`conflictBanner`、`base_updated_at` 乐观锁）。
- 分享链接、公开页、Granola 导入、日历事件、markdown 镜像落盘。
- tiptap 富文本编辑器（1703 行的 `NoteEditor` 里大半是分享/权限/日程，编辑器本身另有 `RichTextEditor`）。

## 3. 本项目落地方案

### 3.1 现状盘点

| 能力 | 位置 | 复用方式 |
|---|---|---|
| SQLite 迁移范式 | `services/history_service.rs:156-260`（WAL + `user_version` + 每次开连接） | 笔记库照抄这套：`notes.sqlite3`、独立 `NOTES_SCHEMA_VERSION` |
| 录音链路 | `commands/audio.rs:500 start_recording` / `:514 stop_recording`，`RecordingContext` 常驻根部 | **不能直接复用**：见下方「耦合点」 |
| 流式转写 | `transcription-result` 事件（`interim` 标记 + `sessionId`） | 仅作为可选的实时模式，一期不用 |
| 采集原语 | `audio_service::spawn_audio_capture_thread(stop_flag, samples, device)`（返回设备采样率） | **笔记录音直接用它**：只采集、不转写 |
| 重采样 | `audio_service::resample::ResamplerState`（`push_i16` 增量重采样到 16k） | 录音时按块重采样后再落盘，文件直接可用 |
| 整段转写 | `funasr_service::transcribe`（WAV bytes → 临时文件 → IPC，已带长音频自适应超时）；`commands/audio_import.rs:183 transcribe_imported_audio`（解码 → 块式流式 → 存历史） | 新增窄接口「文件 → 文本」`transcription_service::transcribe_file`，直接传路径复用已有预算，见 §3.3 |
| 历史与音频 | `transcription_history` 表 + `history_audio/*.wav` + 启动时清孤儿（`history_service.rs:810`）+ 租约（启动清空） | 笔记音频**独立放 `notes_audio/`**，历史行 `audio_file` 留空、只做文本记录；不复用历史 GC |
| LLM 调用 | `services/llm_client/*`（OpenAI / Anthropic / 兼容端点 SSE）、`assistant_service::generate_content` | 摘要生成走同一套 provider 配置与凭据 |
| Markdown 渲染 | `react-markdown` + `remark-gfm` + `katex`（已依赖） | 摘要与笔记预览直接渲染 |
| 模块级 store 范式 | `hooks/audioImportStore.ts`（`useSyncExternalStore`） | 笔记页状态与「生成中」状态放模块级，切页不丢 |
| 契约登记 | `formal/verification.json` + `scripts/check_formal.py` | 新命令必须登记，否则 inventory 不过 |

**录音链路的耦合点**（决定笔记录音不能直接调 `start_recording`）：

- `start_recording_with_start_owner` 在采集启动后**必定**二选一 spawn `spawn_native_recording`（流式 ASR）或 `spawn_interim_loop`，没有「只采集」分支。
- 收尾由 `finalize_recording` 负责，里面绑定了字幕窗口、浮层结果、润色、粘贴、`transcription_history` 写入等一整套听写语义；`finalize.rs:288` 的注释明确写了「原生结果不退化为整段 ASR」。
- 因此笔记录音**另起一条采集会话**（不与听写状态机共用 `RecordingSlot`），停止后走「落盘 WAV → 存历史 → 整段转写」，与听写链路零交叉，回归面最小。

**前端缺口**：没有富文本编辑器（无 tiptap / prosemirror / slate），只有 `react-markdown`。

**能力缺口**：录音只走 `cpal` 默认输入设备（`audio_service/capture.rs:209`），**没有系统声环回**（`wasapi` 已在 `Cargo.toml:91`，但未使用）。线上会议只能录到本机麦克风。

### 3.2 数据模型（`notes.sqlite3`）

```sql
CREATE TABLE note_folders (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  name       TEXT    NOT NULL,
  sort_order INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE UNIQUE INDEX idx_note_folders_name ON note_folders(name);

CREATE TABLE notes (
  id                  INTEGER PRIMARY KEY AUTOINCREMENT,
  folder_id           INTEGER REFERENCES note_folders(id) ON DELETE SET NULL,
  title               TEXT    NOT NULL DEFAULT '',
  content             TEXT    NOT NULL DEFAULT '',   -- 手写笔记（Markdown）
  transcript          TEXT,                          -- 转录纯文本
  summary             TEXT,                          -- AI 摘要（Markdown）
  summary_model       TEXT,
  summary_prompt      TEXT,
  summary_generated_at INTEGER,
  summary_source_hash TEXT,                          -- 素材指纹，判断摘要是否过期
  kind                TEXT    NOT NULL DEFAULT 'personal', -- personal | meeting | import
  duration_sec        REAL,
  history_id          INTEGER,                       -- 关联 transcription_history.id
  audio_file          TEXT,                          -- 录音文件名（notes_audio 内）；先有音频后有 history_id
  created_at          INTEGER NOT NULL,
  updated_at          INTEGER NOT NULL
);
CREATE INDEX idx_notes_folder  ON notes(folder_id, updated_at DESC);
CREATE INDEX idx_notes_updated ON notes(updated_at DESC);
```

与 openwhispr 的差异：

- `transcript` 存**纯文本 + 空行分段**，不存 JSON segments —— 本项目 ASR 不返回时间戳与说话人，JSON 结构没有信息量。
- 去掉 `participants` / `enhanced_at_content_hash`（改名 `summary_source_hash`）/ `note_type`（改名 `kind`，语义更窄）。
- 去掉 `space_id`、`cloud_*`、`share_*`、`deleted_at`（一期不做回收站，删除即删，见 §6 风险）。
- 音频不落 `history_audio/`：那里在 `initialize()` 时会做孤儿清理（`history_service.rs:297-303` 先 `DELETE FROM history_audio_leases`，再 `cleanup_orphan_audio_files_with_connection`），未被历史行引用又无租约的文件会被直接删掉——「录完还没点转文字就重启」会丢录音。笔记音频因此独立放 `notes_audio/`，回收责任归笔记自己，见 §3.3。

### 3.3 后端改动清单

新增 `services/notes_service.rs`（约 600–700 行，含单测）：

```
init() / migrate()                -- 建库（不预置目录，空态引导用户创建）
list_folders() / create_folder(name) / rename_folder(id, name)
delete_folder(id)                 -- 目录内笔记 folder_id 置 NULL
reorder_folders(ids)
list_notes(folder_id: Option, query: Option, limit, offset)
get_note(id) / create_note(folder_id, title, kind) / update_note(id, patch) / delete_note(id)
move_note(id, folder_id)
attach_transcript(note_id, history_id)   -- 从历史记录生成笔记
set_transcript(note_id, text, duration_sec, history_id)
```

新增 `services/note_summary_service.rs`（约 350–450 行）：

- 素材拼装：标题 + 手写笔记 + 转录，按 `MAX_PART_TOKENS ≈ 12288`（本项目按字符估算：中文 1 字 ≈ 1 token，非中文 4 字符 ≈ 1 token）切块，优先在段落/句号边界切。
- 单块提示：只写本段要点，不带标题、不做全文总结、不编造人名与数字。
- 合并提示：汇总各段为固定章节（概要 / 讨论 / 决定 / 待办）。
- 流式输出 `note-summary-delta` 事件 + `note-summary-status`（preparing / part i/n / merging / done / failed / cancelled）。
- 取消：`AppState` 里挂 `CancellationToken`，复用现有 SSE 流取消路径。

#### 转写：加一个「文件 → 文本」的窄接口，不做全量收口

**判断**：现有三处分派不是「抄错了」，而是三种不同的执行策略——

| 位置 | 策略 | 为什么存在 |
|---|---|---|
| `finalize.rs:968 do_final_asr` | 重采样 → 在线/本地一次性 | 听写收尾兜底 |
| `audio_import.rs:89 transcribe_import_streaming` | 1 秒块流式喂 | 上传页要 interim 实时预览（产品特性） |
| `history.rs:162 transcribe_wav_bytes_dispatched` | WAV bytes → 分派 | 历史「重新识别」 |

把它们合成一个带 `progress` / `on_interim` 回调的万能函数，省下的只是每处 6 行 `match`，代价是把听写、上传两条主流程的回归面绑在一起，读起来还要先理解一堆可选参数——**不划算，不做**。

**真正该复用的底层件**（读代码确认）：

- `funasr_service::transcribe` → `transcribe_wav_bytes_via_path` → `send_command_to_server_for_audio(..., wav_duration_secs(bytes))`，**已经带了长音频自适应超时**（就是上轮修 37 分钟失败的那条预算曲线）。笔记的一次性转写不会重演超时问题，无需新造。
- Python 端 `ServerCommand::Transcribe` 支持 `audio_path`，现在只是被 `transcribe_wav_bytes_via_path` 用来落临时文件。**笔记可以直接传已有文件路径**：不读字节、不写临时文件、不复制音频。

所以只加一个窄接口：`services/transcription_service.rs`（约 120 行）

```rust
/// 文件 → 文本。不落库、不写历史、不复制音频。
pub async fn transcribe_file(
    state: &AppState,
    app: &tauri::AppHandle,
    path: &Path,
    duration_sec: Option<f64>,   // 录音侧已知时长，省一次整文件读取
    error_prefix: &str,
) -> Result<TranscriptionResult, AppError>
```

- 本地引擎 → 新增 `funasr_service::transcribe_path(state, app, path, duration_sec)`（约 25 行，直接下发 `audio_path`，路由与预算同 `transcribe_wav_bytes_via_path`）
- 在线引擎 → `check_engine_limit`（从 `audio_import.rs` 搬进 service）→ `glm_asr_service` / `alibaba_asr_service`
- 不动 import 的流式路径：它的 interim 是上传页的产品特性，删它是另一个需求

配套小改动：

| 位置 | 改动 |
|---|---|
| `services/funasr_service.rs` | 新增 `transcribe_path`（复用 `send_command_to_server_for_audio`） |
| `services/transcription_service.rs` | 新增：分派 + 在线上限 + 错误归一到 `AppError` |
| `commands/audio_import.rs` | `check_engine_limit` 改为调用 service 版（单一来源），行为与文案不变 |
| `services/history_service.rs` | **不动**：笔记音频在 `notes_audio/`，历史行的 `audio_file` 传 `None`；私有的 `safe_audio_path` / `cleanup_audio_if_unreferenced` 也无需提权 |
| `utils/paths.rs` | 新增 `notes_audio_dir()`；把 `safe_audio_path` 里的「单段文件名」校验抽成 `safe_file_in_dir(dir, name)`，历史与笔记共用（历史行为不变） |

`do_final_asr` / `transcribe_wav_bytes_dispatched` 保持原样；真要统一留到 P5，且前提是「上传页放弃 interim 预览」。

#### 录音只采集（新增 `services/note_recording_service.rs`，约 250–300 行，含单测）

```
start(state, app, note_id) -> NoteRecordingState   -- 校验麦克风/互斥，起采集线程
  · spawn_audio_capture_thread(stop_flag, samples, device)   -- 复用现有原语
  · 目标文件：`notes_audio/{note_id}-{millis}.wav`（`paths::notes_audio_dir()`）
  · 每 30 秒：取出新增 PCM → ResamplerState 转 16k → 追加写入 wav
  · 只 emit note-recording-state（计时 + 电平），不调用任何 ASR
stop(state, app) -> NoteRecordingResult { audio_file, duration_sec }
  · join 采集线程 → 回填 WAV 头 → 写 notes.audio_file / duration_sec
  · 到此为止：不转写，等用户点「转文字」
cancel(state)  -- 停止并删除文件（用户取消录音）
```

`notes_audio/` 的回收完全归笔记自己：删笔记删文件、删目录级联删、启动时清理「未被 notes 引用」的残留。崩溃留下的半截录音头是占位值，`wav_duration_secs`（`audio_service/wav.rs`）已能按真实剩余字节裁剪，启动清理时直接补写长度即可复用。

新增 `services/audio_service/wav_writer.rs`（约 80 行 + 单测）：占位头写入 → 按块追加 → 关闭回填长度。现有 `wav.rs::encode_wav` 只能一次性编码整段，长会议必须增量写。

#### 笔记转写：一次窄接口调用（新增 `transcribe_note_recording`）

命令里只有编排：

```
audio_file = notes.audio_file                 -- 没有录音就报「这条笔记还没有录音」
result = transcription_service::transcribe_file(
             state, app, notes_service::note_audio_path(audio_file)?,
             Some(note.duration_sec), "转写失败")
history_id = history_service::insert(HistoryDraft {
                 mode: "note", workflow: "note",
                 audio_file: None,               -- 音频归笔记，历史只做文本记录
                 ... })
notes_service::set_transcript(note_id, text, duration_sec, Some(history_id))
```

命令约 25 行，只做「取路径 → 调转写 → 写历史 → 回填笔记」。

复用清单与改动量：

| 复用点 | 现状 | 要做什么 |
|---|---|---|
| `funasr_service::transcribe_wav_bytes_via_path` 的发送与预算逻辑 | 已带长音频自适应超时 | 抽出 `transcribe_path`，去掉临时文件那两步 |
| `ServerCommand::Transcribe { audio_path }` | Python 端已支持 | 直接下发笔记录音路径，**不读字节、不复制** |
| `check_engine_limit` | `audio_import.rs` 私有 | 搬进 `transcription_service`，在线引擎共用 |
| `history_service::insert` + `HistoryDraft.audio_file` | `audio_file: Option<String>`，可传 `None` | 笔记写历史时传 `None`：音频在 `notes_audio/`，历史侧不需要也读不到它 |
| `transcription-result` interim 事件 | 导入用负数 `sessionId` | 笔记**不发** interim（一次性转写本来就没有中间结果），因此不存在与上传页争抢的问题 |

要点：

- **音频只有一份，且在笔记自己的目录**：录音直接写 `notes_audio/{note_id}-{millis}.wav`，`notes.audio_file` 存名字，转写只把绝对路径传给引擎，全程不复制、不读字节。为什么不用 `history_audio/`：那里在 `initialize()` 里会清孤儿（且启动时租约表被清空），未转写、没被历史行引用的录音会被删掉。代价是笔记要自己负责回收：删笔记/删目录/启动清残留三处。
- **历史行不带音频**：`audio_file = None`，历史页能看到这条记录、能导出文本，播放按钮自然不出现（`audio_available` 由 `safe_audio_path` 判定，天然为 false）。需要在历史详情注明「录音在笔记中」，避免用户以为丢了。
- **`mode`/`workflow` 取 `"note"`**：历史页会多一类记录（可查看、可导出文本，但不可播放）。`ensure_reprocessable_workflow` 目前只放行 `"dictation"`，笔记不做历史页「重新识别」，改用笔记自己的「重新转写」，避免两套语义打架。
- **「只录音、未转写」是正常状态**：列表与详情要能区分三种态（有录音无文本 / 转写中 / 已转写），主按钮文案随之切换（转文字 / 重新转写 / 生成笔记）。
- **失败处理不引入新列**：转写失败只提示错误、音频与笔记不变，重试幂等；重启后按钮回到「转文字」，语义仍然正确。
- **互斥**：`AppState` 增加 `note_capture` 槽。听写启动时检查它并返回「笔记正在录音，请先结束」；笔记录音启动时反向检查 `recording.recording`。两路同时开会抢同一个麦克风。
- **没有百分比进度**：一次性转写拿不到块级进度，别做假进度条。UI 显示「已用时 01:23 · 预计约 3 分钟 · 可离开本页」，按 19× 实时给个预估区间即可；真需要百分比时再走分块流式（P5）。

新增 `commands/notes.rs`（约 320 行）+ 在 `commands/mod.rs`、`lib.rs` 注册：

| 命令 | 说明 |
|---|---|
| `list_note_folders` / `create_note_folder` / `rename_note_folder` / `delete_note_folder` / `reorder_note_folders` | 目录 |
| `list_notes` / `get_note` / `create_note` / `update_note` / `delete_note` / `move_note` | 笔记 |
| `create_note_from_history` | 历史记录 → 笔记 |
| `start_note_recording` / `stop_note_recording` / `cancel_note_recording` / `get_note_recording_state` | 只录音 |
| `transcribe_note_recording` | 按钮触发：`transcribe_file` + 写历史 + 回填笔记 |
| `generate_note_summary` / `cancel_note_summary` | AI 摘要 |
| `save_text_file` | 通用「弹保存框 + 落盘」，导出笔记时用；内容由前端按界面语言拼好，后端不解释文本 |

约定：`update_note` 只写传入字段（`Option<Option<T>>` 表达「不改」与「清空」）；`updated_at` 由后端统一刷新；标题留空时列表展示「未命名笔记」由前端兜底。

**不做**的事（避免回归）：`start_recording` / `stop_recording` 签名与行为完全不动，不传 `note_id`，听写链路零改动。

### 3.4 前端改动清单

| 文件 | 改动 |
|---|---|
| `src/main.tsx` | `View` 加 `"notes"`；`navItems` 加「笔记」（`lucide` 的 `NotebookPen`）；slice 分组与懒加载同步 |
| `src/pages/NotesPage.tsx` | 新建：两栏布局（左侧栏 248px + 右编辑器）；空态、加载态 |
| `src/components/notes/NotesSidebar.tsx` | 新建：目录树，**笔记直接挂在目录下面**（参考 openwhispr），目录可折叠、悬停出行内操作（新建笔记/重命名/删除/排序），顶部搜索框 |
| `src/components/notes/NoteEditor.tsx` | 新建：标题输入、元信息行（目录 / 时间 / 保存态）、tab（我的笔记 / 转录 / 摘要）；转录 tab 提供复制与导出 txt，摘要 tab 导出 Markdown |
| `src/components/notes/MarkdownEditor.tsx` | 新建：编辑 / 预览分段切换（textarea + `MarkdownContent`，预览态才挂载渲染器） |
| `src/hooks/notesStore.ts` | 新建：模块级 store（目录树 + 当前笔记 + 保存状态 + 摘要生成状态），切页不丢 |
| `src/hooks/noteRecordingStore.ts` | 新建：模块级 store（录音中/转写中、已录时长、电平、转写开始时间、临时错误），切页不丢 |
| `src/hooks/useNoteSummary.ts` | 新建：订阅流式摘要事件，暴露 `isGenerating` / `progress` / `text` |
| `src/components/notes/RecordingBar.tsx` | 新建：录音状态条（计时 + 电平 + 停止）；停止后同位置换成「转文字」按钮，转写中显示已用时 + 预估时长 + 「可离开本页」提示 |
| `src/api/tauri.ts` | 新增上述命令的 invoke 封装 |
| `src/types/index.ts` | `NoteFolder` / `NoteRecord` / `NoteSummaryStatus` 类型；`PersistentHistoryRecord` 加 `noteId?` 反查 |
| `src/i18n/{zh,en}.ts` | `app.navNotes`、`notes.*`（约 40 个 key） |
| `src/styles/pages.css` | 笔记页布局、目录树、编辑区（预计 +350–450 行） |
| `formal/verification.json` | 19 个新命令 + 3 个事件登记（`note-recording-state`、`note-summary-delta`、`note-summary-status`） |

交互要点（从 openwhispr 学到的）：

- 切页不丢：状态放模块级 store；生成摘要时切走仍继续，回来能看到进度与结果（本次上传页踩过这个坑）。
- 每个 tab 独立编辑器实例（`key`），避免撤销栈跨 tab 互相覆盖。
- 自动保存：输入停止 500–800ms 落库，切笔记/切页/失焦时强制 flush；保存失败要给可见提示，不能静默丢字。
- 摘要过期提示：`summary_source_hash` 与当前素材不一致时，在摘要 tab 顶部显示「转录已更新，可重新生成」。
- 录音态与其他态互斥：录音中「生成笔记」置灰（没有转录可依据）；转写中允许编辑手写笔记，转录 tab 只显示已用时与「可离开本页」。
- 先录后转的预期管理：录音期间转录 tab 不留空白，显示「录音中 · 结束后可转文字」+ 计时；停止后按钮就位，避免用户以为功能坏了或以为已经自动在转。
- 有录音未转写要有痕迹：列表项与转录 tab 顶部标「未转写」，否则用户关掉页面就再也想不起来。

### 3.5 会议流（最终目标）：先录后转，手动触发转写

```
笔记页「开始录音」→ start_note_recording（只采集，wav 边录边落盘，切页不断）
   → 录音中：转录 tab 显示「录音中 · 结束后可转文字」+ 计时/电平
   → 「停止」→ stop_note_recording：回填 wav 头，写 notes.audio_file + duration_sec
   → 转录 tab 显示「转文字」按钮（此时笔记已有录音，可以直接播放/留存）
   → 点「转文字」→ transcribe_note_recording：调 transcription_service::transcribe_file（只传路径）
   → 转写完：notes.transcript + history_id 落库，历史页出现一条 mode="note" 记录
   → 用户点「生成笔记」→ generate_note_summary → 流式写入「摘要」tab
```

**为什么默认先录后转**：实时转录要求全程握着 ASR 推理（本地引擎 19× 实时，约每秒音频 52ms 推理，但引擎常驻、CPU/GPU 持续唤醒），长会议对笔记本电池不友好；且实时结果对「会后生成摘要」没有增量价值——摘要本来就基于最终全文。先录后转把算力从「会议时长」压缩到「会议时长 ÷ 19」，而且只在用户真的需要时才付。

**转写做成手动按钮的三个好处**：录错/录空的不浪费一次转写；用户可以先听录音确认再转；失败重试的语义天然是「再点一次」，不需要引入「待转写」这种中间状态。

代价与补偿：

| 代价 | 补偿 |
|---|---|
| 会议中看不到文字 | 转录 tab 明确提示「结束后可转文字」，计时可见；一期不提供中途预览 |
| 转写要等（1 小时会议约 3 分钟） | 用户主动触发才等；显示已用时 + 预估区间 + 可切页/切笔记；完成后 toast |
| 可能忘了点「转文字」 | 详情页与列表标出「有录音未转写」，与「有转录无摘要」的提示同一套机制 |
| 录音期间只有麦克风电平，没有语义反馈 | 电平条 + 时长，足够判断「录上了」 |

**已知限制**：

- 录音只有麦克风（`audio_service/capture.rs:209` 用 `cpal` 默认输入设备），没有系统声环回。线下会议、或线上会议外放音箱时可用；戴耳机开线上会议只能录到自己。`wasapi` 依赖（`Cargo.toml:91`）已在，后续加第二路输入 + 混音即可，不阻塞本方案。
- 整段转写仍受引擎侧约束：本地引擎在 `finish` 时要把全部 PCM 拼起来，内存随音频线性增长（1 小时约 230MB）。**一期建议单次录音 ≤ 2 小时**；更长的会议需要「按 30 分钟分段转写再拼接」（P5 可选，见 §4）。
- 在线引擎（GLM 30 秒 / DashScope ≈3.9 分钟）显然不适合会议录音，笔记页在检测到在线引擎时给一条「会议录音建议切换本地引擎」的提示，不阻塞。


### 3.6 与历史记录的关系

- 笔记录音在**点「转文字」时**写入 `transcription_history`（`mode="note"`，与听写/导入同级），历史页可播放、可导出；只录音没转写时历史页不会出现半成品记录。历史页「重新识别」暂不放行 `note`（`ensure_reprocessable_workflow` 只认 `dictation`），重转走笔记自己的按钮。
- 笔记通过 `history_id` 反向引用，音频不复制。历史记录被删除时，笔记保留文本、`history_id` 置空。
- 附加价值：历史页每条记录加「存为笔记」，让笔记功能在会议录音做完之前就能用（导入音频 → 存为笔记 → 生成摘要）。

## 4. 分阶段工作清单

### P0 地基：数据层 + 目录 + 笔记 CRUD（已完成 2026-11-18）

- [x] `notes_service.rs`：建库迁移、目录 CRUD、笔记 CRUD、搜索（`LIKE` 标题/正文/转录）
- [x] `commands/notes.rs` + 注册 + `formal/verification.json`（11 命令 + `notes` 契约）
- [x] `notesStore.ts` + `api/tauri.ts` 封装
- [x] `NotesPage` 骨架 + `NotesSidebar`（两栏：目录树内直接列出笔记；新建目录、新建笔记、重命名、删除、排序、折叠、搜索）
- [x] i18n + CSS 基础布局
- [x] 附带：标题/正文的基础编辑与 600ms 防抖自动保存（原计划在 P1，但没有它无法验证落库）
- 验收：能建目录、建笔记、改名、删除、切换后数据仍在；重启应用后数据仍在
- 实现说明：不预置「个人/会议」目录（避免中英文两种语言下写死文案），首次进入为空态引导；目录重名与空名前后端都拦；搜索对 `%`、`_` 做了 `LIKE` 转义

### P1 编辑器与转录展示（已完成 2026-11-18）

- [x] `NoteEditor`：标题、元信息行、tab 切换（我的笔记 / 转录 / 摘要；换笔记回到正文 tab）
- [x] `MarkdownEditor`：编辑 / 预览分段切换，预览态才挂载 `MarkdownContent`；自动保存沿用 P0 的 600ms 防抖 + flush
- [x] 转录 tab：只读展示、字数、复制、导出 txt；摘要 tab：渲染 Markdown、导出完整笔记
- [x] 空态与加载态（无选中、加载中、无转录、无摘要四种）
- 验收：输入后 1 秒内落库，强制退出不丢字；长文本不撑爆布局
- 实现说明：导出文档由**前端**按当前界面语言拼装（标题 + 正文 + 转录 + 摘要），后端 `save_text_file` 只负责弹框与落盘，避免中英文两套界面下后端写死中文小节名；预览复用 `MarkdownContent`，编辑态不付公式预处理的开销
- 偏移：导出从 P5 提前到这里（转录 txt 与整篇 Markdown 共用同一条命令），P5 只留全局搜索与拖拽等打磨项

### P2 历史/上传 → 笔记

- [ ] `create_note_from_history` 命令
- [ ] 历史页每条记录「存为笔记」入口
- [ ] 上传页结果卡「存为笔记」
- 验收：37 分钟录音 → 存为笔记 → 转录完整、音频仍可播放/定位

### P3 会议录音 → 笔记（先录后转，手动触发转写）

阶段一：只录音

- [ ] `wav_writer.rs`：占位头 + 分块追加 + 关闭回填（含单测）
- [ ] `paths::notes_audio_dir()` + `safe_file_in_dir()`（把 `history_service::safe_audio_path` 里的单段名校验抽出来共用）
- [ ] `notes_service` 的音频回收：删笔记 / 删目录级联 / 启动清 `notes_audio/` 里未被引用的残留（含崩溃半截录音补写长度）
- [ ] `note_recording_service.rs`：采集会话、每 30 秒重采样落盘、时长、停止/取消、`AppState.note_capture` 互斥
- [ ] 4 个命令（`start/stop/cancel/get_state`）+ `note-recording-state` 事件 + `formal/verification.json` 登记
- [ ] `RecordingBar`：计时 / 电平 / 停止 / 取消；停止后写 `notes.audio_file` + `duration_sec`
- 验收：录 10 分钟可随时切页；停止后音频文件存在且能播放；**录音后直接重启应用，文件仍在**；「有录音未转写」状态在列表与详情可见

阶段二：手动转写（窄接口，不复制音频）

- [ ] `funasr_service::transcribe_path`：直接下发 `audio_path`（约 25 行，复用现有预算与超时）
- [ ] `transcription_service::transcribe_file`：分派本地/在线 + 在线上限 + 错误归一（约 120 行）
- [ ] `check_engine_limit` 迁入 service，`audio_import.rs` 改为调用它（行为与文案不变）
- [ ] `transcribe_note_recording` 命令：取路径 → 转写 → `insert(mode="note", audio_file=None)` → `set_transcript`
- [ ] 「转文字」按钮 / 已用时与预估 / 失败提示 / 幂等重试（重复点不产生第二条历史）
- 验收（功能）：同一段录音点两次「转文字」结果一致、历史只有一条；转写中切页回来仍在跑
- 验收（无复制）：转写前后 `notes_audio` 与 `history_audio` 都不新增文件；历史行 `audio_file` 为 `NULL`，历史详情提示「录音在笔记中」
- 验收（超时）：1 小时录音转写成功，预算日志与导入同一条曲线（`ceil(dur*0.1+120)` 封顶 900）
- 验收（耗电）：录 10 分钟时 ASR 子进程不活跃、CPU 接近空闲；对照首页实时转录持续占用
- 验收（容错）：转写阶段断网/切引擎/引擎报错，音频仍在，按钮回到「转文字」可重试；在线引擎超限给明确文案而不是泛化失败

### P4 AI 摘要生成（手动触发）

- [ ] `note_summary_service.rs`：素材拼装、分块、单块提示、合并提示
- [ ] 流式事件 + 进度 + 取消
- [ ] 摘要 tab 渲染、「生成笔记」按钮、重新生成、过期提示
- [ ] 摘要复制 / 导出 Markdown
- [ ] 可生成条件：转录非空且未在生成中；无 LLM 凭据时按钮禁用并给出口
- 验收：1 小时转录（约 1.5 万字）能分块跑完；中途取消不留下半截脏数据；断网/无凭据有明确报错

### P5 打磨（可选）

- [ ] 全局搜索（`Ctrl+K`，跨标题/正文/转录）
- [ ] 笔记导出 Markdown / txt
- [ ] 删除二次确认 + 回收站（若需要）
- [ ] 目录拖拽排序、笔记拖拽入目录
- [ ] 超长会议分段转写：>2 小时按 30 分钟切片、逐段转写再拼接（规避引擎整段拼 PCM 的内存增长）
- [ ] 可选「边录边转」开关（默认关）：打开后走首页已验证的实时流式链路，代价是持续推理耗电

## 5. 风险与决策点

| 项 | 风险 | 应对 |
|---|---|---|
| Markdown 源码编辑 | 用户不习惯写 Markdown 符号 | 预览默认开启切换；提供加粗/列表/标题三个快捷键即可覆盖多数场景 |
| 长转录摘要 | 无 token 计数器，只能按字符估算，可能超出模型窗口 | 用保守预算（12k 字符/块）+ 失败重试降块；错误提示要具体 |
| 删除语义 | 一期无回收站，误删不可恢复 | 删除前二次确认；若加软删除，成本不高（`deleted_at` + 列表过滤） |
| 摘要质量 | 本地小模型易丢数字与人名 | 沿用 openwhispr 已验证的提示（强调不得编造、保留专名与数字），并在 UI 标注「AI 生成，需核对」 |
| 数据迁移 | 新库独立，不影响历史库 | 无需迁移；首次启动建库并种子目录 |
| 笔记音频无人回收 | `notes_audio/` 不归历史 GC 管，删笔记/崩溃残留会越积越多（1 小时约 115MB） | 三处回收：删笔记删文件、删目录级联、启动扫未被 notes 引用的残留；配套单测 + 一个「磁盘占用」可见性（设置页显示目录大小，可选） |
| 历史记录无音频 | 笔记转写写的历史行 `audio_file=NULL`，历史页播放按钮消失，用户可能以为录音丢了 | 历史详情对 `mode="note"` 显示「录音在笔记中」并可跳转；不复制音频是刻意取舍（否则 1 小时多 115MB） |
| 录音与笔记一致性 | 停止录音时文件写失败，笔记可能只剩时长没有音频 | 先落文件、回填头，再写 `notes.audio_file`；任一步失败就整体回滚并明确报错。转写失败不影响录音与手写笔记 |
| 先录后转的等待感 | 1 小时会议点「转文字」后约 3 分钟没结果，容易被当成失败 | 已用时 + 预估区间 + 完成 toast + 明确「可以切走」；失败文案要区分「录音没录上」与「转写失败」 |
| 忘记点「转文字」 | 笔记长期只有音频没文本，摘要也就无从生成 | 列表与详情标「未转写」，摘要在无转录时给出「先转文字」引导，而不是灰按钮不解释 |
| 一次性转写没有块级进度 | 用户等约 3 分钟看不到进展，误判卡死 | 显示已用时 + 按 19× 实时给预估区间，不造假进度条；真需要百分比时改分块流式（P5） |
| 路径直传的兼容性 | Python 端现有 `audio_path` 由 Rust 写临时文件产生，直接传长路径/中文路径需验证 | 单测 + 手测中文用户目录下的 1 小时录音；异常时回退 `transcribe_wav_bytes`（读字节）一条兜底 |
| 超长会议 | 引擎 `finish` 要拼全部 PCM，2 小时以上内存与超时都吃紧 | 一期界面标注建议 ≤2 小时；P5 做 30 分钟切片转写。wav 已落盘，最坏情况也能事后导入补救 |
| 录音互斥 | 笔记录音与听写热键同时触发会抢同一个麦克风 | `note_capture` 与 `recording.recording` 双向检查，各自给明确错误文案（不静默失败） |
| 手动生成的遗忘成本 | 用户录完就走，笔记一直是空的 | 转录 tab 顶部在「有转录、无摘要」时给一条不打扰的提示与按钮 |

## 6. 一期明确不做

说话人分离与时间戳、系统声采集、会议中实时上屏（默认先录后转，实时模式放 P5 可选开关）、云端同步与多端、团队空间与权限、分享链接、日历事件关联、会议自动检测、模板市场、富文本编辑器、附件与图片、标签体系、停止录音后自动生成摘要。

## 7. 已确认决策（2026-11-18）

1. 目录层级：**单层**，不嵌套。
2. 编辑器：**Markdown 源码 + 预览切换**，不引入 tiptap。
3. 系统声：**一期不做**，只录麦克风；计划里写成已知限制。
4. 摘要时机：**只手动点「生成笔记」**，不做自动生成。
5. 录音方式：**默认先录后转**——录音期间只采集落盘、不跑流式 ASR。理由是长会议全程实时推理耗电明显，而摘要基于最终全文、实时结果没有增量价值。
6. 转写时机：**停止录音只保存文件，用户点「转文字」才转写**。好处是录坏/录空的不浪费算力、失败重试就是再点一次、不用引入「待转写」中间态。
7. 转写复用的粒度：**不做全量收口**。现存三处分派是三种执行策略（听写兜底 / 上传页 interim 预览 / 历史重新识别），合成一个带回调的万能函数，省下的只是每处 6 行 `match`，却把两条主流程的回归面绑在一起。只加一个窄接口 `transcription_service::transcribe_file(path, duration)`：本地引擎经新增的 `funasr_service::transcribe_path` **直接下发文件路径**（Python 端 `ServerCommand::Transcribe` 已支持 `audio_path`，不读字节、不写临时文件），在线引擎走 `check_engine_limit` + 既有在线 service。
8. 音频归属：录音写进**独立目录 `notes_audio/`**，`notes.audio_file` 存文件名，转写只传绝对路径、不复制、不读字节；写历史行时 `audio_file` 传 `None`，历史只保留文本记录。决定依据：`history_audio/` 在 `initialize()` 会清孤儿（`history_service.rs:297-303` 先清空租约表再扫目录），未转写的录音重启即丢；独立目录则回收责任明确归笔记，代价是删笔记/删目录/启动清残留三处要自己实现。
