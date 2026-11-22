# 项目长期约定（会听 Minutes）

## 品牌命名（用户明确要求，不要再改）

- 中文环境：**会听 Minutes**（`src/i18n/zh.ts` → `app.title`）。这是完整产品名，**不要裁成「会听」**。
- 英文环境：**Minutes**（`src/i18n/en.ts` → `app.title`）。
- 安装包 / 安装目录 / 可执行文件：英文 `Minutes`（`productName`、Cargo package name 均为 `minutes`）。
- 正文句子里的简称用「会听」（如「返回会听继续登录」「允许会听使用套餐额度」），不加英文，属于正常中文表达，不算不一致。
- 历史教训：曾把 `app.title` 的「会听 Minutes」当成冗余改成「会听」，被用户否掉。审计文案时不要以「重复」为由删品牌名。

## 改名中刻意保留的旧标识（不要动）

| 项 | 值 | 原因 |
|:--|:--|:--|
| 数据目录 `paths.rs` `APP_IDENTIFIER` | `com.light-whisper.app` | 改则历史/配置/模型缓存全部「消失」，需一次性迁移才可动 |
| `tauri.conf.json` identifier | `com.light-whisper.dev` | WebView2 数据与更新链换家 |
| `KEYRING_SERVICE` | `light-whisper` | 改则已存 API Key 读不出 |
| localStorage key ×10 | `light-whisper-*` | 改则用户主题/热键/麦克风选择重置 |
| GitHub repo 地址、`LIGHT_WHISPER_*` 环境变量 | — | 内部契约，对用户不可见 |

## 已修复的相关功能点（改回会坏）

- `user_profile.rs` 划词排除项默认值含 `minutes.exe`（否则划词助手弹在自己窗口上）；但老用户 `user_profile.json` 里仍是 `light-whisper.exe`，**迁移 `excluded_apps` 仍未做**，是已知遗留风险。
- `capabilities/main.json` 必须有 `core:window:allow-set-title`：`core:window:default` 只含读标题，不含写，缺了 `setTitle` 会静默失败。
- 窗口标题随语言切换靠 `main.tsx` 里的 `getCurrentWindow().setTitle(t("app.title"))`，`tauri.conf.json` 的静态 title 只是启动瞬间的兜底。
- ASR IPC 超时是「无进展窗口」（2026-11-22 改造）：Python 每 15s 发 `{"type":"progress"}`，Rust 每次 stdout 行重置窗口；`long_audio_*` 预算参数/逻辑已删，不要再按音频时长算总预算。进度消息带 `request_id`，必须在 JSON 值层过滤，否则会被误当最终响应；写 stdin 也必须有独立窗口，否则管道写满会永久挂起。

## 工程约定

- 二进制名由 Cargo `package.name` 决定（dev 与打包都是 `minutes.exe`）；`mainBinaryName` 默认 null，**不要加**（官方建议改 package name）。
- **文档目录约定**：`docs/` 根目录放源码自带的既有技术文档（`r2t2-native.md`、`privacy.md`、`development.md` 等，不要动）；**新写的方案文档一律放 `docs/neo/`**。
- 新 Tauri 命令须同步登记 `formal/verification.json`（commands / command_implementation / contracts evidence），否则 `scripts/check_formal.py` 失败。
- 数据库层测试按既有惯例写 `*_with_connection` 变体，用内存库单测。
- **只有一个库文件 `minutes.sqlite3`**（`src/db.rs`）：历史与笔记同库不同表，代码边界仍由 `history_service` / `notes_service` 承担。
- **建表一律走 `db::ensure_schema(connection, |tx| { ... })`**：只建表，无版本号、无迁移链。应用还没分发，表结构变了直接删库重建，不要再引入 `schema_meta` / `PRAGMA user_version` / 版本号常量这类机制。
- `notes` 建表前必须先 `history_service::ensure_schema`，因为 `history_id` 外键的父表要先存在（SQLite 建表时就校验），两步各自开事务、不能嵌套。建表函数统一叫 `ensure_schema`（不叫 migrate，因为不做迁移）。
- 本项目 rusqlite `bundled` 构建 `SQLITE_DEFAULT_FOREIGN_KEYS` 默认是 1（外键一直生效），但 `db::configure` 仍显式 `PRAGMA foreign_keys = ON`，不要依赖编译期默认值。
- 笔记侧 `history_id` 都是真外键 `REFERENCES transcription_history(id) ON DELETE SET NULL`：历史被保留期淘汰只断引用，笔记不受影响；删笔记不删历史（审计日志）。
- **跨模块写必须一个事务**：`notes_service::create_note_from_history`（读历史 + 建笔记）与 `notes_service::save_recording_transcription`（写历史 + 写段转录）都是单个 `IMMEDIATE` 事务。命令层不要再「先 `history_service::get` 再 `notes_service::xxx`」，那样中间能被保留期 GC 插进来。保留期清理（`history_service::cleanup_expired_with_connection`）自己开写事务，只能放在提交之后。
- `notes` 表不持有音频引用，音频只在 `note_recordings.audio_file`；`notes(history_id)` 有唯一索引，一条历史只能转出一篇笔记。
- 音频目录两套、生命周期不同：`history_audio/`（历史，有租约与 GC）与 `notes_audio/`（笔记，按 `note_recordings.audio_file` 引用回收）。路径解析一律走 `paths::safe_file_in_dir`。
- 会议录音（`note_recording_service`）只落盘不转写，落盘间隔 500ms；录音期间**不应出现 ASR 子进程**，这是「先录后转省电」的验收点。
- 会议录音与听写共用麦克风，互斥只做单向（会议录音开始前查听写），不要在听写热键链路里加反向检查。

## 笔记转写（P3 阶段二）

- 所有 ASR 分派只走 `services/transcription_service.rs`：唯一入口 `transcribe(app, state, AudioInput, TranscribeOptions)`，按 `AudioInput::{WavBytes, File}` + 时长选 `Whole` / `Chunked`。命令层不要再自己 match 引擎。2026-11-21 已删掉 `Streaming` 策略（它只服务上传页的渐进文字）。
- 笔记转写**必须直传路径**（`AudioInput::File`），不要为了「统一」「省一次解码」把一小时 WAV 读进内存。2026-11-20 的约束是「Python 端整段读入 + PCM 全量驻留」；改流式反而更费：Rust 侧要持 i16（1 小时 115MB），Python 的 `_chunks` 再留 float32（230MB），`finish` 里 `concatenate` 峰值再加一份。
- 长录音靠**切片**控内存，不靠流式：`WavChunker` 流式读文件，峰值恒定单块。上传页的实时预览也已于 2026-11-21 去掉（见 `unified-transcription-service-plan.md` §11），现在只有实时听写用原生流式。
- 上传也走「先落盘再转写」：`save_audio` 排在 `transcribe` 之前，音频路径直接交给转录层，失败再 `discard_audio` 回滚。**不要改回 `WavBytes`**——那会让 Rust 侧多留一份整段 WAV，长音频还失去切片。
- 幂等规则：`transcribe_note_recording` 默认只转 `transcript` 为空的段；**转写失败不写历史**（成功才 insert），所以重试永远只有一条历史行。`force=true` 是「重新转写」，会覆盖已有转录并**就地更新**该段原来的历史行（不追加重复记录）。
- 历史行的 `mode="note"` 且 `audio_file=NULL`：音频归笔记，写进历史会让历史 GC 删掉笔记唯一的录音。
- 并发保护是后端 `AppState.note_transcription` 原子占位，不要只靠前端按钮 `disabled`。
- 「重新转写」与「历史 → 笔记跳转」已在 P5 落地：`transcribe_note_recording(note_id, force)`、`find_note_by_history` / `find_notes_by_history`（批量，列表渲染用）。反查要查两条路径：`notes.history_id` 与 `note_recordings.history_id`。
