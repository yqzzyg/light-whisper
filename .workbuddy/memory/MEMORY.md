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

## 说话人分离（P0 已落地，2026-11-25）

- 入口是 `transcription_service::transcribe_with_speakers`，**不是**新命令：`transcribe_note_recording` 内部换函数名，降级全在服务层。命令层不要写 `match`。
- **锁顺序是硬约束**：`native_asr_owner` 不可重入，必须「先判条件（不拿锁）→ 需要降级当场降级 → 真要分离才拿锁」。写反即 `lock→降级→transcribe→再 lock` → 永久挂起。
- 分段只有**一条写路径** `notes_service::save_recording_transcription_with_segments`：空 `segments` 表示「这次没分段」，重转先删后插，降级时顺带清掉旧分段（否则界面的人跟正文对不上）。
- 前端渲染**必须按 `recordingId` 分组**：每段录音时间轴都从 0 开始，拍平会重叠且出现两个「说话人 1」。跨录音合并说话人需要声纹档案，明确不在 P0。
- 分离模型是**两个非 gated 的 HF 镜像仓库**（`csukuangfj/sherpa-onnx-pyannote-segmentation-3-0`、`csukuangfj/speaker-embedding-models`）。计划文档里「pyannote 只能走 GitHub release tar.bz2」是错的——当时只是 huggingface.co 连不上。
- 聚类阈值不要用 openwhispr 的 0.5：同一个人跨 31 分钟余弦 0.973、两个不同人 0.683，`cluster_distance_cut=0.40` / `merge_centroid_cos=0.85` 才是校准值。
- 性能默认值：分段滑窗位移 2.0s + 嵌入窗步长 4，实测 101 秒/小时；**批处理不可行**（CAM++ batch 8 比 batch 1 更慢）。
- **ASR 单次成本随音频长度超线性**（本机 3050 Ti 实测）：30s=25 ms/s、60s=63.6 ms/s、110s=161 ms/s。所以 `max_segment_seconds` 从 115 降到 **45**（2026-11-26），不能按「不分块上限」定段长；选 45 不选 30 是因为 30s 只快 7 秒（ASR 73.0→66.1s）却多 17 段（106→123）。注意 bench 的 ms/s 是纯长段值，整体加权后 45s 档实测 40.1 ms/s（短段固定开销摊高）。
- `adjacent_merge_gap_seconds=1.0` 曾是空阈值：`extract_segments` 已把 ≤0.5s 缝隙接上，真人停顿要么 <0.5s 要么 ≥1.15s，中间区间不存在。2026-11-26 改 2.0（175 段 → 91 段，中位 5.7s → 9.9s），计划文档 §9.7。
- **阈值不做用户配置**（2026-11-26 定，文档 §9.8）：17 个参数全是「模型 + 机器」校准值，失败是静默分错人，不给用户调。三层机制：① `DiarizeOptions` 是唯一真源（`frame_gap_seconds` / `vote_threshold` / `overlap_frame_share` / `embed_frame_max_local_speakers` 已从函数体和默认参数收进来）；② `LIGHT_WHISPER_DIARIZE_OVERRIDES`（JSON，未知键/越界/坏 JSON 一律 ValueError + 记 ERROR，带 `locked` 集合优先于自适应）；③ `adapt_options` 按后端/显存/时长推 `max_segment_seconds` 与 `embed_window_stride`。
- 自适应规则：CUDA + 显存 ≥8GB → 段长上限 60，否则 45；再叠「60s + 3% 时长」预算（用拟合 `ms≈2.203L²−96L+1646`），2h 音频在 12GB 上也退回 45。**嵌入步长固定 4，绝不随设备/时长放粗**（2026-11-27 主修完成，文档 §9.9）：曾按「非 CUDA 或 >45min → 6，>90min → 8」推导，分段窗长 10s、每 2s 一个，步长 6 → 选中窗间隔 12s，留下 2s 没有任何声纹参与投票的覆盖缺口，这些帧不活跃、不生成段，逐段 ASR 直接跳过 → 吞字。58 分钟党课实测 299 个缺口、743s 语音被跳过，「全党高度团结统一」整句被吞；修复后按 `max_safe_embed_stride(W,S)=W//S-1`（本模型 5→4）动态钳制，`adapt_options` 只封顶不放粗，`_embed_windows` 运行时硬钳制（locked=8 也压回 4），`_vote` 显式要求 `cover>0`。同一份音频重跑：94 段（原 303）、`speech_ratio=0.9677`、整句恢复。改这条规则时务必用 >45min 音频回归，31 分钟测不到。
- 踩坑：`Qwen3ASRServer._get_gpu_device_info(self)` 曾把基类 `(*, include_memory=False)` 签名收窄，通用调用方一传参就 TypeError，被 `transcribe_diarized` 的 except 吞成「分离降级」——界面只表现为没做分离。**子类覆盖基类方法必须保留完整签名**。
- `_get_gpu_device_info` 的缓存必须区分「查过显存」与「只查了名字」：`initialize()` 先跑一次 `include_memory=False`，否则后续要显存的调用永远拿到无 `gpu_memory_total` 的旧字典，自适应静默失效。
- **新增任何耗时长命令都必须登记进 `gpu_idle.GPU_IDLE_ACTIVITY_ACTIONS`**：漏了就不持 `_gpu_idle_lock`、也不刷新活动时间，看门狗会在命令中途卸载运行时，随后 `session.run` 打到野指针上 → access violation 崩进程。`transcribe_diarized` 就是漏网之鱼（2026-11-26 修，计划文档 §6.5）。
