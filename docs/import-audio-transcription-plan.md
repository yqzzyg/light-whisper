# 导入音频转写功能 — 详细实施方案

> 状态：待实现 · 最后更新：2026-11-16
> 需求来源：「先录后转」经澄清后确认为**导入外部音频文件转写**，非延迟录音转写。

## 1. 需求定义

| 项 | 决策 |
|---|---|
| 功能 | 从本地选择音频文件，转写为文字 |
| 入口 | 左侧栏「上传」导航项 → 独立的上传页面（见 4.7） |
| 格式 | WAV / MP3 / M4A / FLAC / OGG |
| 处理 | 单文件、只出 ASR 原文、不润色 |
| 结果交付 | 本地引擎边转边在上传页结果卡逐句预览（流式 interim），在线引擎转完一次性预览；可编辑、可复制，同时写入历史；不自动粘贴、不回灌主结果区 |
| 解码方案 | `symphonia`（纯 Rust），不引入 ffmpeg |
| 长音频 | 本地引擎原生自动分块，一期直通；在线引擎超限时明确拦截 |

## 2. 现状调查结论

### 2.1 可复用能力

| 能力 | 位置 | 说明 |
|---|---|---|
| 文件选择对话框 | `commands/funasr.rs:614` `pick_folder` | `rfd::AsyncFileDialog`，`Cargo.toml:83` 已依赖 |
| 引擎分发 | `commands/history.rs:158-176` `transcribe_saved_audio` | 按 `alibaba-asr` / `glm-asr` / 默认本地分流，可直接抽取复用 |
| WAV 编码 | `services/audio_service/wav.rs:5` `encode_wav` | `hound` 写 16bit PCM，单声道 |
| 重采样 | `services/audio_service/resample.rs` | `rubato 0.16` 已依赖，支持任意采样率 → 16k |
| 历史落盘 | `services/history_service.rs:312` `save_audio` + `HistoryDraft` | 音频存 `history_audio/{ms}-{session_id}.wav`，防目录穿越、原子写、GC 租约齐全 |
| 历史刷新 | `history-updated` 事件 | 历史页已订阅 |

### 2.2 关键约束

**音频格式**：Rust 侧只有 `hound`（仅 WAV 编解码），无任何通用解码器；`symphonia` 未进入 `Cargo.lock`。所有 ASR 引擎入口统一接收 **16bit PCM WAV 字节**：

- `funasr_service.rs:613 transcribe(state, Vec<u8>, app)` — 写临时文件交 Python
- `glm_asr_service.rs:73 transcribe(state, Vec<u8>)`
- `alibaba_asr_service.rs` — WAV base64 请求体

**引擎时长/大小上限**：

| 引擎 | 上限 | 来源 |
|---|---|---|
| 本地 Qwen3 / R2T2 | 无硬上限 | `qwen3_native.py:21-23`：≤115s 单次推理，>115s 由 `audio.cpp` 的 `audio_chunk_mode=auto` 按连续 30s 分块；单次 `session_run`、模型不重载。测试见 `test_qwen_native_long_audio.py` |
| GLM-ASR | 25MiB 且 **30 秒** | `glm_asr_service.rs:12-13` |
| DashScope | 10MB 请求体（base64 放大 4/3 → 原始 ≈7.5MB ≈ 3.9 分钟） | `alibaba_asr_service.rs:14` |

**结论**：切分只有在线引擎需要，本地引擎已内置。不需要 ffmpeg。

### 2.3 项目特有的 CI 门禁

`scripts/check_formal.py:check_inventory` 强制要求：

1. `src-tauri/src/lib.rs` 的 `generate_handler!` 命令集合 == `formal/verification.json` 的 `commands` 键集合（多一个少一个都失败）
2. 每个命令在 `command_implementation` 中有且仅有精确锚点 `文件#fn 函数名(`
3. 每个 `contracts` 绑定必须非空且含 `obligation/models/implementation/evidence/assumptions`
4. `events` 集合由源码扫描得出，必须与清单一致

**因此新增命令必须同步 `formal/verification.json`，否则 CI 首个 job「Application formal contracts」失败，`release.sh` 卡住。**

## 3. 架构设计

### 3.1 数据流

```
首页「导入音频」按钮
  → pick_audio_file()          [Rust: rfd 选择文件，返回路径]
  → transcribe_imported_audio(path)
      → 读取文件字节 + 大小预检
      → symphonia 解码 → PCM
      → rubato 重采样 16k 单声道
      → hound 编码 WAV
      → 时长/大小校验（按引擎上限）
      → 引擎分流：
          ├─ 本地（qwen3/r2t2）：NativeStreamClient start → 按 1 秒喂 PCM
          │    → 每块 emit transcription-result（interim=true、负数 sessionId）
          │    → finish 拿最终结果   ← 上传页边转边出字
          └─ 在线（GLM/DashScope）：一次性转写，转完才出结果
      → save_audio() 保存原始 WAV
      → history_service::insert(status=success, workflow=import, text=原文)
      → emit history-updated
  → 命令返回历史记录
  → hook 保存 result，上传页复用 TranscriptionResult 就地预览
  → 用户直接复制 / 编辑后复制；历史列表由 history-updated 自动刷新
```

### 3.2 刻意规避的设计

| 不做 | 原因 |
|---|---|
| interim 用负数 sessionId emit `transcription-result` | 复用现有事件做流式预览，但 sessionId 取负数：浮层只处理非负数、主界面丢弃所有 `interim=true`，两者都不会把导入字幕显示出来；不撞 `useRecording.ts` 的 session 过滤逻辑，也不用新增事件、`formal/verification.json` 无需改动 |
| 不加新事件 | 复用已有 `transcription-result` 的 interim 通道表达流式进度，避免动 events 清单 |
| 不用时间戳当原生会话 id | 常驻 Python 引擎要求 `stream_start` 的 session id **严格大于上一次**（`r2t2_stream.py:97`）。导入若用毫秒时间戳，会把引擎上限抬到万亿级，之后录音的 1、2、3… 全被判为旧会话并报 `stream response reported failure`。必须与录音共用 `native_recording.rs::NEXT_NATIVE_SESSION`（`next_native_session_id()`），按持锁顺序单调分配 |
| 不加新 DTO | `src/types/generated/ipc.ts` 由 ts-rs 生成且有严格比对测试（`ipc_dto_contract_tests.rs:48`），命令层直接返回 `HistoryRecord` 即可复用 |
| 不自动粘贴 | 用户选择；同时避开原应用上下文已丢失的问题 |
| 不润色 | 用户选择；`polish_ms` 留 `None` |

## 4. 详细改动清单

### 4.1 Rust 依赖（`src-tauri/Cargo.toml`）

```toml
# 通用音频解码（导入文件 → PCM）
symphonia = { version = "0.5", features = ["mp3", "flac", "wav", "isomp4", "ogg", "vorbis"] }
```

> `symphonia` 为纯 Rust，`isomp4` 覆盖 M4A/AAC，`ogg`+`vorbis` 覆盖 OGG。无外部二进制依赖。加入后需更新 `Cargo.lock`。

### 4.2 Rust 服务层（新文件 `src-tauri/src/services/audio_import_service.rs`）

职责：文件 → 16k 单声道 WAV。

关键函数签名：

```rust
/// 支持的扩展名白名单
pub const SUPPORTED_EXTENSIONS: &[&str] = &["wav", "mp3", "m4a", "flac", "ogg"];

/// 解码任意支持格式为 16bit PCM，返回 (samples, sample_rate)
pub fn decode_file(path: &Path) -> Result<(Vec<i16>, u32), AppError>;

/// 统一入口：解码 → 重采样 16k 单声道 → hound 编码 WAV
pub fn import_audio_to_wav(path: &Path) -> Result<ImportedAudio, AppError>;

pub struct ImportedAudio {
    pub wav: Vec<u8>,
    pub duration_sec: f64,
}
```

实现要点：

1. 用 `std::fs::metadata` 做源文件大小预检（建议上限 200MB，防御解压炸弹式超大文件）
2. `symphonia::core::probe::Hint` 从扩展名推断格式，`symphonia::default::get_probe()` 打开
3. 取第一个 audio track，遍历 packets 送 codec decoder，收集 `AudioBuffer<i16>` 或 f32
4. 多声道混单声道（取平均），采样率非 16k 时用 `resample.rs` 的 `Resampler` 转 16k
5. 用 `wav::encode_wav(&samples, 16000)` 编码
6. `duration_sec = samples.len() as f64 / 16000.0`
7. 错误处理：不支持的扩展名 → `AppError::Asr("不支持的音频格式…")`；解码失败 → `AppError::Asr("音频解码失败…")`

模块注册：`services/mod.rs` 加入 `pub mod audio_import_service;`

### 4.3 Rust 命令层（新文件 `src-tauri/src/commands/audio_import.rs`）

```rust
/// 选择本地音频文件，返回路径（复用 funasr.rs:614 pick_folder 写法）
#[tauri::command]
pub async fn pick_audio_file(window: tauri::WebviewWindow)
    -> Result<Option<String>, AppError>;

/// 导入并转写，返回历史记录（复用 HistoryRecord DTO）
#[tauri::command]
pub async fn transcribe_imported_audio(
    app_handle: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<history_service::HistoryRecord, AppError>;
```

`transcribe_imported_audio` 流程：

1. 路径校验（存在、扩展名白名单、大小预检）
2. `audio_import_service::import_audio_to_wav(Path::new(&path))`
3. 引擎上限检查：
   - 读取 `paths::read_engine_config()`
   - `glm-asr`：时长 >30s 或 WAV >25MiB → `AppError::Asr` 带明确中文提示
   - `alibaba-asr`：字节 base64 后 >10MB → 同上
   - 本地：不拦截时长
4. 复用分发转写：从 `commands/history.rs` 抽取 `transcribe_wav_bytes_dispatched(app, state, wav) -> Result<TranscriptionResult, String>` 公共函数（`transcribe_saved_audio` 与新增命令共用）
5. 失败处理：构造 `HistoryDraft { status: "asr_error", error: Some(msg), workflow: "import", text: "", original_text: "", audio_file: None, ... }` 入库，返回该记录（这样失败也留痕，符合现有 `finalize.rs:428-497` 的错误分支惯例）
6. 成功：
   - `save_audio(session_id, wav.clone())` → `audio_file`（session_id 用 `state.recording.session_counter` 新分配，避免与录音会话碰撞）
   - `HistoryDraft { status: "success", workflow: "import", text: 原文, original_text: 原文, language, engine, duration_sec, asr_ms, total_ms, audio_file, polish_ms: None, ... }`
   - `history_service::insert(draft, retention_days)`
   - `emit_history_updated(&app_handle, Some(id))`
   - 返回记录

命令注册：`src-tauri/src/lib.rs` 的 `generate_handler!` 加两行。

模块注册：`src-tauri/src/commands/mod.rs` 加 `pub mod audio_import;`

### 4.4 CI 契约同步（`formal/verification.json`）

必须同时改三处（缺一 CI 挂）：

1. `commands` 段新增两个键，各绑定已有契约标签（建议 `["history"]` 或 `["engine"]`，需与 `contracts` 中已有键对应）：
   ```json
   "commands::audio_import::pick_audio_file": ["<合适契约>"],
   "commands::audio_import::transcribe_imported_audio": ["history"]
   ```
2. `command_implementation` 段新增：
   ```json
   "commands::audio_import::pick_audio_file": "src-tauri/src/commands/audio_import.rs#fn pick_audio_file(",
   "commands::audio_import::transcribe_imported_audio": "src-tauri/src/commands/audio_import.rs#fn transcribe_imported_audio("
   ```
3. 若新增契约标签，需在 `contracts` 段定义完整（含五个必填字段）；优先复用已有 `history` 契约避免新增。

> IPC 白名单 `ipc_policy.rs` 默认放行 `main` 窗口所有命令，新命令无需改白名单；但应保留其默认行为，不要显式加到 subtitle/selection 白名单。

### 4.5 前端 API（`src/api/tauri.ts`）

在 `pickFolder()`（:701）附近新增：

```ts
export function pickAudioFile(): Promise<string | null> {
  return invokeCommand<string | null>("pick_audio_file");
}
export function transcribeImportedAudio(path: string): Promise<PersistentHistoryRecord> {
  return invokeCommand<PersistentHistoryRecord>("transcribe_imported_audio", { path });
}
```

### 4.6 前端 Hook（新文件 `src/hooks/useAudioImport.ts`）

状态机：`idle → picking → importing → done | error`

```ts
interface UseAudioImportReturn {
  isImporting: boolean;
  /** 最近一次成功转写的记录，供上传页直接预览 */
  result: PersistentHistoryRecord | null;
  /** 本地引擎流式转写中的实时文本，仅导入中非空 */
  interimText: string | null;
  importAudio: () => Promise<void>;
}
```

**状态放在模块级 store（`src/hooks/audioImportStore.ts`），hook 只做订阅适配。** 左侧栏切换时只挂载当前页面，`UploadPage` 会被卸载；状态若跟随组件，结果和进行中的转写都会丢失。store 用 `useSyncExternalStore` 暴露快照，组件卸载不影响转写，切回上传页仍能看到结果与进度。

订阅 `transcription-result`，只接收 `interim=true` 且 `sessionId < 0` 的事件（导入串行，负数 sessionId 只可能属于本轮），写入 `interimText`；订阅挂在模块上、不随页面卸载取消，在每轮 `importAudio` 开始时建立、结束时释放。

`importAudio` 流程：
1. `pickAudioFile()`；取消则 return（保留上一次结果）
2. `isImporting = true`；选到文件后先清空 `result` / `interimText`，避免展示过期结果
3. `transcribeImportedAudio(path)`；本地引擎期间 `interimText` 随事件逐句更新
4. 成功 → 写入 `result`，由上传页就地预览（不再弹成功 toast）
5. `asr_error` / 命令异常 → `toast.error(t("toast.importAudioFailed", { error }))`
6. `finally` → 清空 `interimText`、`isImporting = false`

> 单测需在 `beforeEach` 调用 `resetAudioImportStoreForTests()`，否则模块级状态会跨用例残留。

> 历史列表刷新由 `history-updated` 事件驱动，无需手动 reload。

### 4.7 前端 UI（左侧栏「上传」导航 + 独立页面）

入口按用户提供的参考截图确定：**左侧栏新增「上传」导航项**（位于「历史记录」之后），点击进入独立的上传页面。页面布局参考截图的「上传」页：
- 顶部标题栏显示「上传」
- 中部一个大卡片作为上传/拖放区，含选择文件按钮
- 导入中显示进度，完成后结果写入历史（按需求不改主结果区）

**这不是首页里的按钮，也不是标题栏图标，而是一个新的顶层页面。**

涉及三个文件：

**① `src/main.tsx`（导航 + 路由）**

现状：
- `type View = "main" | "settings" | "history"`（:17）
- 侧边栏分两组：`navItems.slice(0, 2)` 为功能组（首页、历史记录），`navItems.slice(2)` 为底部设置组（:140、:154）
- 页面路由 `:170-176` 三分支

改动：
```ts
type View = "main" | "settings" | "history" | "upload";
const loadUploadPage = () => import("./pages/UploadPage");
const UploadPage = lazy(loadUploadPage);

const navItems = [
  { id: "main", labelKey: "app.navHome", icon: Home },
  { id: "history", labelKey: "app.navHistory", icon: History },
  { id: "upload", labelKey: "app.navUpload", icon: Upload },   // 新增，功能组
  { id: "settings", labelKey: "app.navSettings", icon: Settings },
];
```
- 功能组改为 `navItems.slice(0, 3)`（首页、历史记录、上传），底部设置组改为 `navItems.slice(3)`
- 路由新增分支：`view === "upload" ? <UploadPage onNavigate={navigateTo} animClass={animClass} /> : ...`
- 预加载计时器（:78-83）加入 `loadUploadPage()`
- `navigateTo` 的 `loadTarget` 分支（:118）加入 upload（否则懒加载等待逻辑会漏）
- 图标 `Upload` 从 `lucide-react` 引入（项目已在 `SystemSettingsSections.tsx:2` 用过，无新依赖）

**② 新文件 `src/pages/UploadPage.tsx`**

结构参考截图但遵循本项目 DESIGN.md（不照搬截图里的蓝色按钮和产品文案）：
```
page-root upload-page
 ├─ TitleBar title={t("upload.title")}
 └─ page-content
     ├─ upload-card              ← 大卡片，居中
     │   ├─ 图标 + 说明文字
     │   ├─ 选择文件按钮（ghost/主按钮按 DESIGN.md）
     │   └─ 导入中：进度 / 加载态
     └─ upload-result                    ← 有结果时显示，页面可滚动
         ├─ TranscriptionResult          ← 复用首页结果卡：可编辑文本框 + 复制按钮 + 统计
         │   └─ 文本框默认最高 min(42vh, 380px)，超出在卡内滚动，
         │      并出现「展开全部 / 收起」；展开后由页面滚动承载完整内容
         └─ 结果提示文案
```
- 复用 `useAudioImport` hook（4.6 节）的 `isImporting` / `result` / `importAudio`
- 有结果时主按钮文案切换为「再传一个」，点击直接开始下一轮导入
- 成功不再弹 toast：结果就在页面内，同时写入历史；不回灌主结果区、不自动粘贴
- 拖放支持作为可选增强（一期可先只做点击选择）

**③ `src/styles/pages.css`**

新增 `.upload-page` / `.upload-card` 样式，遵循 DESIGN.md：抬升表面、16px 圆角、14px 内边距、结构性细边框、`--shadow-card`；深浅主题共用 token。不新增装饰性渐变或玻璃效果。

**禁用/并发**：`UploadPage` 内导入时按钮进入 loading 态；由于上传页与录音页互斥（不同路由），无需与录音状态做 disabled 联动——这是比原「首页按钮」方案更干净的地方。

### 4.8 类型（`src/types/index.ts`）

`:285` 的 `workflow` 联合类型加 `"import"`：

```ts
workflow: "dictation" | "assistant" | "edit" | "import";
```

> `HistoryPage.tsx:353` 的 `canReprocess = record.workflow === "dictation"` 无需改——import 记录不显示重识别按钮，符合「只出原文」定位。

### 4.9 i18n（`src/i18n/zh.ts` / `src/i18n/en.ts`）

新增 key：

| key | zh | en |
|---|---|---|
| `app.navUpload` | 上传 | Upload |
| `upload.title` | 上传 | Upload |
| `upload.hint` | 选择音频文件，转写为文字并存入历史 | Select an audio file to transcribe into history |
| `upload.selectFile` | 选择音频文件 | Select audio file |
| `upload.importAnother` | 再传一个 | Import another |
| `upload.importing` | 正在导入… | Importing… |
| `upload.resultHint` | 结果已存入历史，可直接复制或编辑后复制 | Saved to history. Copy it here, or edit before copying. |
| `toast.importAudioFailed` | 导入失败：{{error}} | Import failed: {{error}} |
| `toast.importAudioTooLarge` | 音频过长，当前引擎仅支持约 {{seconds}} 秒 | Audio too long; current engine supports ~{{seconds}}s |

## 5. 测试计划

### 5.1 Rust 单测（新文件 `src-tauri/src/services/audio_import_service/tests.rs` 或内嵌）

| 用例 | 断言 |
|---|---|
| 各格式解码 | 用 `hound` 生成 WAV 样本；MP3/FLAC 等可用最小合法样本（fixture）解码成功 |
| 重采样 | 44.1k 立体声 → 16k 单声道，样本数与时长正确 |
| 不支持格式 | `.txt` / `.exe` 返回错误 |
| 损坏文件 | 截断 WAV 返回解码错误而非 panic |
| 超大文件预检 | 超过阈值直接拒绝，不进入解码 |
| 命令层上限拦截 | GLM 引擎下 31s 音频返回明确错误 |

### 5.2 前端测试

- `src/hooks/__tests__/useAudioImport.test.tsx`：mock 两个 invoke，覆盖取消/成功（保存 result 且不弹成功 toast）/新一轮清空旧结果/失败/并发重复点击/负数 sessionId 流式 interim/**卸载后重新挂载仍保留结果**/**卸载期间转写继续并在切回后出结果**
- `src/pages/__tests__/UploadPage.test.tsx`：上传页渲染、选择文件按钮点击调用 hook、导入中 loading 态与禁用、结果预览与复制、「再传一个」
- `src/main.test.tsx`（若存在，否则新建）：左侧栏出现「上传」导航项、点击切换到上传页、路由渲染 UploadPage

### 5.3 CI 门禁验证

```bash
python scripts/check_formal.py          # 契约 inventory 校验
pnpm check:rust                          # fmt + clippy
pnpm test                                # vitest + perf
pnpm check                               # 全量前端检查
```

## 6. 风险与边界

| 风险 | 处置 |
|---|---|
| symphonia 增大安装包 | 纯 Rust，增量几百 KB 级，可接受；`scripts/check_bundle_budget.mjs` 会卡体积预算，如超需调整 |
| 在线引擎上传超限 | 解码后按时长/大小拦截，给明确中文提示，不静默失败 |
| 内存占用（长音频整段解码） | 源文件预检上限 200MB；本地引擎长音频由底层分块，但 PCM 数组仍全量驻留，1 小时 ≈ 115MB，可接受 |
| symphonia 对生僻编码覆盖不足 | 一期只承诺 5 种格式；遇到不支持的编码给明确错误，不静默降级 |
| 历史 GC 误删 | 复用现有 `save_audio` + `HistoryDraft`，纳入既有 retention_days 管理 |

## 7. 实施顺序

1. `Cargo.toml` 加依赖 → `cargo update` 更新 lock
2. `audio_import_service.rs` 解码服务 + 单测
3. 抽取 `history.rs` 的 `transcribe_wav_bytes_dispatched` 公共函数
4. `commands/audio_import.rs` 两个命令
5. `lib.rs` / `commands/mod.rs` / `services/mod.rs` 注册
6. `formal/verification.json` 三处同步
7. 前端 `api/tauri.ts` / `useAudioImport.ts` / 新建 `pages/UploadPage.tsx` / `main.tsx`（导航项+路由）/ `styles/pages.css` / `types/index.ts` / i18n
8. 前端测试
9. 全量 CI 校验

## 8. 二期（暂不实施）

若后续确认需要「在线引擎长音频自动切分」：

- 按引擎上限自适应窗口（GLM 25s、DashScope 3.5min），带 2s 重叠
- 分段转写 → 顺序拼接原文 → 作为一条历史写入
- 与「只出原文」天然契合，段间无需语义连贯
- 不需要 ffmpeg

若后续需要「视频文件提取音频」：届时再评估引入 ffmpeg，这是 symphonia 覆盖不到的领域。
