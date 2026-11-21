# 转录服务统一化 — 方案

> 状态：已实施（阶段 A–D 全部落地） · 最后更新：2026-11-21
> 落地时对设计做了三处收敛，见第 9 节；正文保留设计原貌以便对照。
>
> 起因：上传音频与笔记录音各走一条 ASR 链路，超时预算、切片策略、进度上报都各写一份，
> 排查「5 分钟音频只出 3 个字 / 笔记转写 152 秒超时」时不得不分别读两套代码。

## 1. 背景

`src-tauri/src/services/transcription_service.rs` 本来就是 ASR 分派层，但它只覆盖了
「整段字节 / 整段路径」两种形态。上传链路在它**外面**另开了一条原生流式分支，
笔记链路在它**里面**又叠了一层切片分支，于是同一个能力出现了三处实现。

实测数据（同一台机器、同一 `qwen3-asr-0.6b`、同一条 317 秒音频）：

| 链路 | 噪音过滤 | 耗时 | 字数 |
|---|---|---|---|
| 上传（原生流式） | 开 | 365 秒 | 3 |
| 上传（原生流式） | 关 | 68.6 秒 / 71.1 秒 | 1466 |
| 笔记（路径直传） | 开 | 超时（152 秒上限） | 0 |

结论与本次重构无关，但说明了分叉的代价：**同一个故障要在两条链路上分别验证**。
本次只做结构收敛，不改音频增强器。

## 2. 现状：分叉点

| 重复项 | 上传 | 笔记 |
|---|---|---|
| 引擎分派 | `src-tauri/src/commands/audio_import.rs:55` `uses_native_streaming` | `src-tauri/src/services/transcription_service.rs:29` `is_local_engine` |
| 拿 ASR 锁 | 外层 `native_asr_owner.lock()`（`audio_import.rs:174`） | 内层，由 `transcribe_path` 自己拿（`funasr_service.rs:709`） |
| 长音频策略 | 无（不切片） | `src-tauri/src/services/audio_chunk_service.rs:34` `should_chunk` |
| 进度上报 | `audio_import.rs:61` `emit_import_interim` | `note_transcription.rs:201` `note-transcription-progress` |
| 错误文案 | `"导入转写失败: {e}"`（半角冒号） | `"{prefix}：{e}"`（全角冒号） |
| 历史落库 | `mode=import` + `audio_file` | `mode=note` + `audio_file=NULL` |

上传的分支点：

```rust
// src-tauri/src/commands/audio_import.rs:169
let asr_result = if uses_native_streaming(&engine) {
    transcribe_import_streaming(...)          // 自己持锁、自己喂块、自己发事件
} else {
    transcription_service::transcribe_wav_bytes(...)   // 走统一层，内部再拿一次锁
};
```

同一个命令里，两条分支的**持锁位置**都不一样，这是最需要收敛的地方。

## 3. 目标设计

### 3.1 唯一入口

`transcription_service` 增加一个入口，上传与笔记都只调它，各自只保留落库：

```rust
pub enum AudioInput<'a> {
    /// 内存里的 16k 单声道 16bit WAV 字节（上传解码结果、历史重识别）
    WavBytes(&'a [u8]),
    /// 磁盘上的音频文件（笔记录音、以及需要按文件切片的长音频）
    File(&'a Path),
}

pub enum Preview {
    None,
    /// 上传页逐段实时文本
    ImportInterim,
    /// 笔记切片进度
    ChunkProgress { note_id: i64, recording_id: i64 },
}

pub struct TranscribeOptions<'a> {
    /// 已知时长；`WavBytes` 留空时从 WAV 头补。
    pub duration_sec: Option<f64>,
    pub error_prefix: &'a str,
    pub preview: Preview,
}

pub async fn transcribe(
    app: &tauri::AppHandle,
    state: &AppState,
    input: AudioInput<'_>,
    options: TranscribeOptions<'_>,
) -> Result<TranscriptionResult, AppError>;
```

`WavBytes` 借用切片而不是 `Vec<u8>`：上传解码一次就把同一份字节交给转写与落盘，
不必为了所有权再拷贝上百兆。不引入 `Pcm16` 变体——切片块由 `Chunked` 策略内部用
`WavChunker` 读文件后直接喂 `transcribe_pcm16_locked`，调用方不需要构造 PCM。

### 3.2 分发规则集中在一处

```rust
fn pick_strategy(input: &AudioInput, opts: &TranscribeOptions) -> Strategy {
    // 切片依赖 WavChunker 在文件里找静音落刀，所以只对路径输入生效。
    if should_chunk(opts.duration_sec) && matches!(input, AudioInput::File(_)) {
        return Strategy::Chunked;
    }
    if matches!(opts.preview, Preview::ImportInterim) {
        return Strategy::Streaming;
    }
    Strategy::Whole
}
```

在线引擎在 `transcribe` 里先行分流（`dispatch_online`），不进 `pick_strategy`——
它们的策略永远是「整段字节」，没有选择余地。三种策略对应现有实现，逻辑不重写，
只改调用位置：

| 策略 | 现行实现 |
|---|---|
| `Whole` | 文件走 `funasr_service::transcribe_path_locked`，字节走 `transcribe_locked` |
| `Streaming` | 原 `audio_import.rs` 的 `transcribe_import_streaming`（迁入） |
| `Chunked` | 原 `note_transcription.rs` 的 `transcribe_in_chunks`（迁入） |

### 3.3 预览事件集中在一处

`Preview` 只是调用方的**意图声明**，事件名、payload 形状、负数 sessionId 这类展示层约定
全部收进 `src-tauri/src/services/transcription_events.rs`：

```rust
/// 一次导入的实时文本通道：sessionId 在开通时确定一次，整轮复用。
pub struct InterimChannel { session_id: i64 }
impl InterimChannel {
    pub fn open() -> Self;
    pub fn emit(&self, app: &tauri::AppHandle, update: &NativeStreamUpdate);
}

pub fn emit_chunk_progress(app: &tauri::AppHandle, preview: Preview, chunk: usize, total: usize);
```

`InterimChannel` 用结构体而不是自由函数，是因为 sessionId 只在「开通」时算一次：
每块都换 id 会让前端无法判断这些 interim 属于同一次转写。

理由：负数 sessionId 是「只有上传页消费」的路由技巧，属于展示约定。让它留在命令层或
散在 `transcription_service` 里都会把 UI 细节拖进 ASR 层。

### 3.4 锁：拆 `_locked` 变体

现在三处各自拿锁，`tokio::sync::Mutex` 不可重入，统一入口一旦自己持锁，
再调 `transcribe_path` / `transcribe` / `transcribe_pcm16` 会**自锁**。

`funasr_service` 已有无锁内部实现（`transcribe_wav_bytes_via_path`、
`transcribe_pcm16_via_path`），缺的是路径直传这一支。补两个：

```
transcribe_path_locked(state, path, duration_sec, app)      // 新增，无锁
transcribe_pcm16_locked(state, samples, sample_rate, app)    // 新增，无锁
```

`transcribe` / `transcribe_pcm16` 保留为「拿锁 + 调 `_locked`」的薄壳，实时听写
（`audio_service::finalize` / `interim`）与模型探测继续用它们，行为不变。
`transcribe_path` 没有锁外调用方，pub 壳直接删除，只留 `_locked`。
统一入口持锁一次，内部只调 `_locked`。

### 3.5 统一后的调用形态

上传：

```rust
// 解码一次，产物同时用于转写（内存字节）与落盘（历史音频）。
let imported = audio_import_service::import_audio_to_wav(file_path)?;
let result = transcription_service::transcribe(
    &app_handle, state.inner(),
    AudioInput::WavBytes(&imported.wav),
    TranscribeOptions::new("导入转写失败")
        .with_duration(Some(imported.duration_sec))
        .with_preview(Preview::ImportInterim),
).await;
```

笔记：

```rust
transcription_service::transcribe(
    self.app, self.state,
    AudioInput::File(path),
    TranscribeOptions::new(error_prefix)
        .with_duration(recording.duration_sec)
        .with_preview(Preview::ChunkProgress {
            note_id: recording.note_id,
            recording_id: recording.id,
        }),
).await
```

## 4. 两段式上传

独立于第 3 节，可并行做。

### 4.1 新增只读命令

```rust
#[tauri::command]
pub async fn probe_audio_file(path: String) -> Result<AudioProbe, AppError>;

pub struct AudioProbe {
    pub duration_sec: Option<f64>,      // 容器给不出帧数时为 None
    pub size_bytes: u64,
    pub sample_rate: u32,
    pub engine: String,
    pub limit_hint: Option<String>,     // 在线引擎超限时直接给文案
    pub estimated_secs: Option<f64>,    // 在线引擎依赖网络，不给预估
}
```

`duration_sec` 用 symphonia 的 `n_frames` 推算，不落地解码；拿不到就是 `None`，
前端显示「时长未知」而不是退回全解码——那会让这一步从毫秒变成秒级。
上限判断按「16k 单声道 16bit ≈ 32000 字节/秒」估算 WAV 体积，比源文件大小准得多。

### 4.2 前端状态机

`src/hooks/audioImportStore.ts:107` `startAudioImport` 从「选文件 → 立即转」拆成三步：

```
idle ──picker──> picked(path, probe) ──user click──> transcribing ──> done
                        └──────── cancel ────────> idle
```

新增状态：`picked: { path, probe } | null`。UI 在 `picked` 时显示时长、大小、
引擎、预估耗时与「开始转写」按钮；超限时按钮禁用并显示 `limit_hint`。

`src/api/tauri.ts` 增加 `probeAudioFile(path)`。

## 5. 行为变化（需要明确确认）

| 项 | 现状 | 统一后 |
|---|---|---|
| 上传 ≥2 小时音频 | 走流式，7200 次 IPC | 走切片，逐 30 分钟块 |
| 上传流式预览 | 固定开启（本地引擎） | 由 `Preview::ImportInterim` 决定，默认仍开启 |
| 笔记与上传并发 | 各自持锁，实际互斥 | 入口持锁，行为不变 |
| 笔记切片进度文案 | 「正在转写第 N 块」 | 不变 |
| 错误文案冒号 | 半角 / 全角混用 | 统一全角 |

第一条是真实行为变化：长上传不再靠「每块 60 秒预算」硬撑，而是走切片 + 按时长放宽预算。

## 6. 实施清单（全部完成）

**阶段 A — 拆锁（无行为变化）**

1. ✅ `src-tauri/src/services/funasr_service.rs`：补 `transcribe_locked`、`transcribe_path_locked`、
   `transcribe_pcm16_locked`；`transcribe` / `transcribe_pcm16` 保留为薄壳，
   `transcribe_path` 因无锁外调用方直接删除。
2. ✅ 锁的覆盖由「连调两次」的端到端路径保证：上传与笔记均走同一入口，串行执行。

**阶段 B — 迁策略**

3. ✅ `src-tauri/src/services/transcription_service.rs`：`AudioInput` / `Preview` /
   `TranscribeOptions` / `transcribe` / `pick_strategy`。
4. ✅ `transcribe_streaming` 与 `transcribe_chunked` 迁入。
5. ✅ 新增 `src-tauri/src/services/transcription_events.rs`（`Preview`、`InterimChannel`、
   `emit_chunk_progress`）。
6. ✅ 新增 `audio_service::wav::wav_pcm16_samples`：让流式既能吃文件也能吃 WAV 字节，
   上传因此不必为了喂样本再落一份临时文件。

**阶段 C — 改调用方**

7. ✅ `src-tauri/src/commands/audio_import.rs`：删 `uses_native_streaming`、
   `transcribe_import_streaming`、`emit_import_interim` 与三个相关单测。
8. ✅ `src-tauri/src/commands/note_transcription.rs`：`run_asr` 收敛为一次 `transcribe` 调用，
   删 `transcribe_in_chunks` 与进度事件常量。
9. ✅ `pick_strategy` 单测覆盖「≥2 小时切片优先于流式」「只有显式要预览才流式」「短文件整段」。

**阶段 D — 两段式上传**

10. ✅ `probe_audio_file` + `AudioProbe`（camelCase 序列化）；`audio_import_service::probe_audio`
    只读容器头。
11. ✅ `src/api/tauri.ts` 加 `probeAudioFile`；`audioImportStore` 改为
    `selectAudio` / `startImport` / `cancelSelection`；`UploadPage` 增加确认卡片。
12. ✅ 前端单测：`useAudioImport` 14 例、`UploadPage` 15 例，覆盖 picked 态、超限禁用、
    取消选择、失败保留选中。

**契约同步**

13. ✅ `formal/verification.json` 登记 `commands::audio_import::probe_audio_file` →
    `["history"]`（它服务于「是否要落这条历史」的决策），实现锚点指向
    `src-tauri/src/commands/audio_import.rs#fn probe_audio_file(`。命令总数 165 → 166。
14. ✅ 未新增 contract：新命令复用既有 `history` 契约，避免为此扩一套 TLA+ 模型。

## 7. 验证

| 层 | 方式 |
|---|---|
| 策略分发 | `pick_strategy` 纯函数单测：≥2 小时切片优先于流式、只有显式要预览才流式、短文件整段 |
| 锁 | `*_locked` 变体由统一入口持锁一次调用，`tokio::sync::Mutex` 不可重入因此不自行加锁 |
| 端到端 | 同一条 317.7 秒音频分别按两条链路的协议驱动引擎，比对耗时与文本 |
| 门禁 | `cargo test --lib`（994）、`vitest`（1062）、`tsc`、`eslint`、`check_formal.py`（166 命令） |

### 7.1 实测记录（2026-11-21，ThinkPad / MX450 2G / qwen3-asr-0.6b / 增强关闭）

用脚本按 Rust 实际发出的协议直接驱动 `engine.exe`：笔记走
`{"action":"transcribe","audio_path":...}`，上传走 `stream_start` → `stream_feed`
（5120 样本/包，不传 `preview`，与 `NativeStreamClient::feed` 的序列化一致）→ `stream_finish`。

| 轮次 | 链路 | 耗时 | 字数 |
|---|---|---:|---:|
| 冷启动 | 笔记·整段 | 81.1s | 1431 |
| 冷启动 | 上传·流式 | 66.7s | 1431 |
| 同进程 | 整段（热身） | 80.8s | 1431 |
| 同进程 | 上传·流式 | 47.1s | 1431 |
| 同进程 | 笔记·整段（第二次） | 123.1s | 1431 |

结论：

1. **文本逐字一致**。5 次跑全部 1431 字（去空格 1421），统一入口没有引入差异，
   优于原定「字数差异 ≤3%」的判据。
2. **笔记整段路径会随进程复用变慢**：同进程第一次 80.8s、第二次 123.1s，退化 52%。
   原因是整段推理每次为大段音频申请大块 KV，2G 显存下释放后池碎片化，下一次分配更吃力；
   流式用固定窗口增量推理，不吃这个亏（66.7s → 47.1s，反而变快）。
3. 笔记的超时预算是 `dur/10 + 120` = 152s，最坏 123s 只剩 24% 余量。本次不动这条链路
   （用户决定），风险记录在此：显存更小或音频更长时会先撞超时，而不是先慢。

## 8. 边界：这次不做的事

- **不修音频增强器**。它仍有独立问题（流式逐帧输出近乎静音、离线整段 1.27 倍实时），
  与链路结构无关，修它要动 `src-tauri/resources/reuse_enhancement.py` 的增益重建。
- **不动 Python 协议**。`stream_start/feed/finish` 与 `transcribe` 两个 action 都保留。
- **不把历史落库收进转录层**。`mode=import` 带音频、`mode=note` 留空是产品语义差异，
  放进服务层会让「转写」和「归档」耦合。
- **不改在线引擎的行为**。它们本来就不支持流式与切片，统一后只是被显式表达出来。

## 9. 落地时对设计的三处收敛

1. **`LongAudio` 枚举被删除。** 设计里有 `Auto` / `Whole` 两个值，但落地时没有任何调用方
   需要「强制整段」——`WavBytes` 本来就不参与切片。留一个永远传 `Auto` 的字段是空抽象，
   等真有需求再加。

2. **`WholePath` 与 `WholeBytes` 合并为 `Whole`。** 「整段」只有一种语义，传输方式
   （路径直传还是内存字节）由 `AudioInput` 的变体决定，不需要两个策略名。

3. **`AudioInput::WavBytes` 从 `Vec<u8>` 改为 `&[u8]`。** 上传解码一次就要把同一份
   WAV 同时交给转写和 `history_service::save_audio`；用借用可以省掉一次上百兆的拷贝。
   在线引擎需要 owned 字节发 HTTP，那一处 `to_vec()` 无法避免。

另外补了一个 `AppError::message()`：`AppError` 的 Display 自带「ASR错误:」前缀，
跨层拼文案会变成「导入转写失败：ASR错误: xxx」，有它就能剥掉一层。

## 10. 待办：笔记链路是否也改流式

实测表明流式比整段快一倍且不随进程退化，笔记路径若改走
`stream_start/feed/finish`（不推 interim），两条链路才算真正同一条。
代价是流式需要支持从文件按块读（≥2 小时不能整段进内存），
并且要重新评估交给引擎的单会话长度上限。本次未做。
