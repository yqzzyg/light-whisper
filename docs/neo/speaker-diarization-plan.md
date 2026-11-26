# 说话人分离 — 方案

> 一句话：在**本地引擎**的笔记转写上，加一层
> `FireRedVAD 粗筛 → pyannote 说话人分段 → CAM++ 声纹嵌入 → 层次聚类 → 逐段 ASR`，
> 产出带说话人编号的分段转写（含重叠标记）。模型缺失或任一步失败时
> **自动降级**——最差退化成现有整段转写，用户不会因为分离失败而拿不到文字。

适用场景：**面对面会议，单麦克风采集全场混音**。远程会议（系统音频采集）不在本次范围。

三个新增资产：pyannote segmentation-3.0（5.8MB）+ CAM++（27MB）+ 自写聚类（0 依赖）。
FireRedVAD 是项目既有组件，不是本方案引入的（见 §2.8）。

---

## 1. 结论先行

| 决策 | 结论 | 理由 |
|---|---|---|
| 声纹模型 | **3D-Speaker CAM++**，`3dspeaker_speech_campplus_sv_zh_en_16k-common_advanced.onnx` | 同族中文版权重实测 26.7MB，整体约 27MB 量级；Apache-2.0；中文优于 ECAPA-TDNN；ONNX 直跑 |
| 分段 | **FireRedVAD 粗筛 + pyannote segmentation-3.0 说话人分段**（ONNX，sherpa-onnx 分发） | 抢话/串音是面对面场景的核心难点，只有 7 类 powerset 能表达「两人同时说话」；详见 §4.2 |
| 聚类 | **纯 numpy 的 average-linkage 层次聚类**（对象是 ~2400 个「窗-说话人」嵌入） | `sklearn` 在打包排除列表里；超过 1200 点在抽样子集上聚类再全量归簇 |
| 嵌入单位 | **「(10s 窗, 窗内局部说话人)」+ 窗内音频拼接**，不是短段 | 段级嵌入实测不可用：段长中位 1.65s，同人相邻段同簇率仅 56.9%；详见 §9.6 |
| 引擎接入 | **Python 侧一次跑完**，一次 IPC（P0 只做 Qwen3） | 模型已在进程里，逐段回调 Rust 会重复加载与多次 IPC |
| 在线引擎 | **明确不支持**，UI 给提示 | GLM/DashScope 只收整段字节，无法承载分段结果，硬做既慢又贵 |
| 说话人改名 | P2 做，P0/P1 只显示「说话人 1/2」 | 编号能对上就已有价值，改名是纯增量 |
| 跨会议声纹档案 | P3，暂不做 | 需要用户授权与隐私设计，不是分离的前置 |
| 触发方式 | **自动，默认开**，设置可关 | 用户预期就是「录完就有说话人」；关掉即静默走老路径 |
| 时长上限 | **> 2 小时不分离**，转发老切片路径 + UI 明示 | 不做半成品；分离路径不切片，长录音留给后续流式化 |

> **2026-11-24 修订**：本方案初稿判断「不引入 pyannote」，理由（gated 授权、外部二进制、体积）
> 逐条核实后不成立，已改为「用」。修订过程与实证见 §4.2——那里保留了初稿的错误判断，
> 因为**为什么错**比结论本身更值得记住。

> **2026-11-24 第二轮修订（有实测）**：用 31 分钟真实录音跑通了全链路，推翻了两处设计：
> ① 嵌入单位从「短段」改为「窗-说话人」，段级嵌入实测不可用；
> ② 聚类阈值不能照抄 openwhispr，其 0.5 的合并阈值会把两个真人并成一个（实测在距离 0.55 处确实并了）。
> 完整数据见 **§9.6**。性能也从「120s/小时」修正为实测 **~560s/小时**。

**最大的工作量不在模型，在数据模型**——见 §2.2。

---

## 2. 现状核查（都是代码事实，不是推测）

### 2.1 采集：只有麦克风轨

`src-tauri/src/services/audio_service/windows_capture.rs` 里只枚举了采集方向：

```rust
enumerator.get_device_collection(&Direction::Capture)
enumerator.get_default_device(&Direction::Capture)
```

没有渲染/回环方向，也就没有系统音频采集。所以本项目面对的永远是**单轨混音**——
这是问题难度的来源，也是与参考项目 openwhispr 最大的分野（见 §3）。

### 2.2 转写：纯文本，**没有任何分段结构**

这是本方案的主要改造点。当前数据模型里根本没有「段」：

```rust
// src-tauri/src/services/funasr_service.rs 里的 TranscriptionResult
pub struct TranscriptionResult {
    pub text: String,
    pub duration: Option<f64>,
    pub success: bool,
    pub error: Option<String>,
    pub language: Option<String>,
}
```

```sql
-- src-tauri/src/services/notes_service.rs 里的 note_recordings 建表
CREATE TABLE IF NOT EXISTS note_recordings (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    note_id      INTEGER NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
    audio_file   TEXT    NOT NULL,
    duration_sec REAL,
    transcript   TEXT,            -- ← 一整段字符串，无时间戳、无说话人
    history_id   INTEGER REFERENCES transcription_history(id) ON DELETE SET NULL,
    sort_order   INTEGER NOT NULL DEFAULT 0,
    created_at   INTEGER NOT NULL
);
```

前端类型同样是扁平的（`src/types/notes.ts` 的 `NoteRecording.transcript: string | null`）。
所以「说话人分离」在本项目里**不是给现有结构加个字段，而是要引入一层新的结构**。
§5 给出设计，原则是**不动 `transcript` 的语义**，让摘要链路零改动。

### 2.3 VAD 现在只用来裁首尾静音，不产出分段

`src-tauri/resources/qwen3_asr_server.py` 的 `_filter_speech` 把 VAD 的所有段
**合并成首尾一个区间**就扔给 ASR：

```python
start = max(0, int(chunks[0]["start"]))
end = min(len(audio), int(chunks[-1]["end"]))
# Preserve pauses between spoken regions. Qwen still receives natural
# phrase timing; only idle time before/after the utterance is removed.
return np.ascontiguousarray(audio[start:end]), len(chunks), vad_ms
```

也就是说 `FireRedVad.speech_timestamps()` 已经有分段能力，只是被现有调用方主动丢弃了。
**分离功能不需要新写 VAD，只需要换个用法**。

### 2.4 fbank 参数与 CAM++ 的输入要求逐项吻合

`src-tauri/resources/firered_vad.py` 里的 fbank 配置：

```python
fbank_options.frame_opts.samp_freq = SAMPLE_RATE   # 16000
fbank_options.frame_opts.frame_length_ms = 25
fbank_options.frame_opts.frame_shift_ms = 10
fbank_options.frame_opts.dither = 0
fbank_options.mel_opts.num_bins = 80
```

CAM++ 要的就是 16k / 25ms / 10ms / 80 维 fbank。**唯一的差异在归一化**：
FireRedVAD 用固定 CMVN（`fireredvad_cmvn.json`），CAM++ 要**逐句均值归一化**
（3D-Speaker 的 `mean_nor=True`）。这一点在 openwhispr 里有实测警告：

> Per-utterance cepstral mean normalization (3D-Speaker FBank mean_nor=True):
> ... without it embeddings collapse into a narrow, non-discriminative cone.

**不做逐句归一化，声纹会塌成一团、完全区分不出人。** 这是实现时必须守住的一条。

### 2.5 依赖全部已有，不需要新增 Python 包

`pyproject.toml`：

```toml
dependencies = [
    "huggingface-hub>=0.24.0",
    "kaldi-native-fbank==1.22.3",   # fbank 提取
    "numpy<2",                       # 聚类
    "onnxruntime>=1.17.0,<2",        # CAM++ 推理
    ...
]
```

CAM++ 需要的三样东西全在。**这是选纯 ONNX 路线而不是 FunASR 的决定性理由**——
`scripts/build_engine.py` 的排除列表里有 `modelscope`、`sklearn`、`torch`，
FunASR 全家桶会直接让打包炸掉。

### 2.6 引擎协议可扩展

`src-tauri/resources/server_common.py` 的 `_execute_action` 是字符串分派，
加一个 action 是纯增量：

```python
def _execute_action(self, command, action):
    if action == "transcribe":
        return self.transcribe_audio(...)
    if action == "status":
        ...
    return {"success": False, "error": f"未知命令: {action}"}
```

而且 `server_common.py` 里已有「无进展窗口」的进度机制（`{"type":"progress"}`），
长录音的分离进度可以直接复用，不用新造协议。

### 2.7 模型下载已有镜像回落

`src-tauri/resources/download_models.py` 顶部：

```python
DEFAULT_HF_ENDPOINT = "https://huggingface.co"
DEFAULT_HF_FALLBACK_ENDPOINT = "https://hf-mirror.com"
```

且注释写明「用户显式设置 `HF_ENDPOINT` 时尊重其选择，未设置时才自动切镜像」。
CAM++ 的新增下载沿用这套机制即可，不引入 ModelScope 依赖。
（实测 ModelScope 直连也可用，可作为后续备用渠道，但**不作为 P0 依赖**。）

### 2.8 FireRedVAD 的来历：它是既有组件，不是本方案引入的

这一点容易误会，写清楚备查。引入提交：

```
946face  2026-07-31  feat(asr): replace Whisper with FireRedVAD
```

该提交一次性把原来的 Whisper VAD 换成 FireRedVAD，含四件东西：

| 文件 | 说明 |
|---|---|
| `src-tauri/resources/firered_vad.py` | 188 行，adapt 自 FireRedTeam/FireRedVAD |
| `src-tauri/resources/fireredvad_vad.onnx` | 2,388,488 字节，**随包内置、不走下载** |
| `src-tauri/resources/fireredvad_cmvn.json` | 固定 CMVN，80 维 |
| `src-tauri/resources/FireRedVAD-LICENSE.txt` | Apache-2.0 全文 |

`THIRD_PARTY_NOTICES.md` 有专门条目声明模型资产与整合代码的授权归属
（模型与整合部分 Apache-2.0，本项目的原创整合与后处理改动按 GPL-3.0-only）。

也就是说 FireRedVAD 已经在项目里跑了几个月（当前版本 1.7.14），
**本方案只是在它后面接一段，不是替换它**。

---

## 3. 场景差异：面对面 ≠ 远程

参考项目 openwhispr 的决策代码 `meetingDiarizationInput.js` 写得很直白：

> A meeting session records two parallel raw PCM captures: the system-audio tap (calls)
> and the microphone (in-person sessions, kept only until system audio turns audible).
> **At stop, exactly one of them feeds the diarizer and the other is deleted.**

它两轨**二选一**，且只要系统音频出现过就分离系统轨、把麦克风轨删掉。
它的主战场是远程会议——系统轨是数字无损的干净信号，分离是「锦上添花」。
面对面支持是后来补的（#1726），而且原话是：

> Recordings with everyone in the room and no call **ended with zero diarized segments**,
> because only the (silent) system-audio channel was ever diarized.

对本项目而言，麦克风轨是**唯一信息源**，分离不是锦上添花，是**能否知道谁在说话的唯一途径**。
由此产生三个必须正面处理、而 openwhispr 基本不受影响的问题：

| 差异 | 远程（openwhispr 主场景） | 面对面（本项目） | 本方案对策 |
|---|---|---|---|
| 信号质量 | 数字直取，每人一个干净声道 | 全场混在一个麦里，距离/朝向/音量都影响声纹 | 聚类阈值按距离放宽一档；pyannote 的边界比 VAD 准，段质量有保障 |
| 串音/回声 | 几乎没有 | 开外放时扬声器声音被二次拾取，同一人被劈成「空气传播」与「经扬声器」两个簇 | pyannote 检出重叠 → 不误判为换人；AEC + 聚类后回声路径合并；提示戴耳机 |
| 已知人数 | 从参会列表可得 | 不知道几个人 | 自动估人数为默认，UI 允许用户覆盖（P1） |

**注意 openwhispr 的整套后处理策略都隐含「用户会告知参会人数」**（`DEFAULT_EXPECTED_SPEAKER_COUNT: 2`）。
面对面场景没有这个前提，所以自动估人数必须自己扛住，不能把用户填人数当作必需条件。

---

## 4. 目标设计

### 4.1 流水线

```
note_recordings.audio_file (notes_audio/*.wav, 16k mono)
   │
   ├─① FireRedVAD 粗筛：丢纯静音/音乐/噪声             已有，2.3MB
   │      → 候选语音区间 [[start,end], ...]（不合并首尾）
   │
   ├─② pyannote segmentation-3.0：10s 窗 / 1s 步长     新增，5.8MB
   │      对每个候选区间滑窗 → [1,589,7] powerset log-prob
   │      → 局部说话人片段（含 overlap 标记）
   │
   ├─③ CAM++：对每个「(10s 窗, 窗内局部说话人)」取声纹  新增，27MB
   │      把该说话人在窗内的活动帧音频首尾相接（丢掉空隙）
   │      fbank(80) → 逐句均值归一化 → 192 维向量
   │
   ├─④ 聚类：average-linkage on cosine distance        纯 numpy
   │      对 ③ 的 (窗, 局部说话人) 向量聚类，得到全局 speaker_id
   │      再把全局编号按 10s 窗投票摊回每一帧
   │
   ├─⑤ 后处理：幽灵过滤 / 同簇合并 / 坍塌检测 / 短段归并
   │
   ├─⑥ 同说话人相邻段合并 → 段数下降一个量级
   │
   ├─⑦ 逐段 ASR（复用同一进程里已加载的 Qwen3 session）
   │
   └─⑧ 返回 [(speaker_id, start_ms, end_ms, text, overlap), ...] + 拼接文本
```

第 ① 与第 ② 步是**串联互补、不是二选一**：FireRedVAD 只回答「这里有没有人声」，
它的强项是抗音乐与噪声（在人工标注的声学事件库上训练，不是从 ASR 强制对齐里推监督），
所以拿它先把无关区间滤掉，能直接减少 pyannote 要跑多少个 10 秒窗。
pyannote 只在「可能有人说话」的区间上跑，负责「谁、几个人、有没有抢话」。

> 为什么不把 FireRedVAD 换成 pyannote 的 VAD 模式？它也能做 VAD（`VoiceActivityDetection`
> pipeline），但那是拿一个 1.5M 参数的说话人模型去做二分类，比 0.6M 的专用 VAD 慢且无收益。
> 各做各的强项。

### 4.2 分段器选型：为什么最终用 pyannote（附一次判断修正）

**先纠正本方案初稿的判断。** 初稿写「不引入 pyannote」，给了四条理由，逐条核实后四条都站不住：

| 初稿的判断 | 核实结果 |
|---|---|
| HF gated，用户要申请 token | **许可本身是 MIT**。gating 只收集公司/网址等联系信息（`extra_gated_fields: Company/university, Website`），不是法律限制。更关键：**sherpa-onnx 在 GitHub release 上分发了一份非 gated 的 ONNX**（`sherpa-onnx-pyannote-segmentation-3-0.tar.bz2`），完全不需要 HF token |
| 需要额外一个外部二进制 | openwhispr 用 `sherpa-onnx-diarize` 二进制是为了跑完整 pipeline；我们只需要那个 ONNX，**用已装的 onnxruntime 直接跑，同进程、不需要 torch** |
| 收益不抵成本 | 实测 **5.8MB**（tar.bz2 里的 `model.onnx`，fp32），与 27MB 的 CAM++ 同量级 |
| 分段质量只是「更好一点」 | 不是程度差别，是**能力差别**，见下 |

净成本因此变成：+5.8MB + 一个 10 秒窗滑窗循环 + powerset 解码。比初稿描述的低一个档次。

**为什么这是能力差别而不是程度差别：抢话是面对面场景的定义性难点。**

- FireRedVAD 的输出是「有/没有人声」。两人抢话时它给出一段连续语音，**边界信息根本不存在**；
- 「VAD 段内用声纹距离找变化点」在抢话处会失效：混音处的声纹向量是两个人叠加的结果，
  既不像 A 也不像 B，距离会同时跳向两边，判定结果基本是随机的；
- 7 类 powerset（`∅`、`{1}`、`{2}`、`{3}`、`{1,2}`、`{1,3}`、`{2,3}`）是唯一能表达
  「两个人同时说话」的输出形式。

用初稿方案先跑一轮，很可能在真实录音的第一关就卡在串音，再回来换 pyannote——白做一轮。
与其赌「先简单后复杂」能省事，不如一开始就上对的分段器。

**保留 FireRedVAD，但换了角色。** 两者是串联互补：

| | FireRedVAD | pyannote seg-3.0 |
|---|---|---|
| 角色 | 粗筛：丢掉纯静音/音乐/噪声 | 说话人分段：谁、几个人、有无重叠 |
| 强项 | 抗背景音乐与噪声（人工标注声学事件训练） | 说话人边界与重叠检测 |
| 弱项 | 完全不懂说话人 | 段本身不含全局身份，靠后续聚类 |
| 体积 | 2.3MB（已有） | 5.8MB（新增） |

FireRedVAD 的强项正好补 pyannote 的短处——pyannote 虽然也能做 VAD
（`VoiceActivityDetection` pipeline），但那是拿 1.5M 参数的说话人模型做二分类，
比 0.6M 的专用 VAD 慢且无收益。各做各的强项。

### 4.3 分段参数

**第一级：FireRedVAD 粗筛**（已有，Apache-2.0）

- 复用 `FireRedVadOptions`，但**不能**用现有 `speech_pad_ms=120` 的合并逻辑把相邻段缝起来
- 新增一个不合并的取段方法（或在 `speaker_diarize.py` 里调 `probabilities()` 自己走状态机）
- 丢弃 < 0.4s 的段（气口、点头声）

**第二级：pyannote segmentation-3.0**（新增，MIT）

| 项 | 值 |
|---|---|
| 输入 | `[1, 1, 160000]`——一个 10 秒窗，16k 单声道 float32 |
| 输出 | `[1, 589, 7]`——每帧 7 类 powerset 的 log-prob（是 logits，不是概率，别直接当置信度用） |
| 帧长 | `receptive_field_shift = 270` 采样 = **16.875 ms**（从模型 `custom_metadata_map` 读，别写死） |
| 窗策略 | **10 秒窗 / 1 秒步长**（`window_shift = 0.1 × window_size`，由模型元数据决定） |
| 尾窗 | 零填充到 160000 |
| 重叠判定 | 单人类别与双人类别概率都过阈值时，标 `overlap=true` |

两条实现要点：

1. **窗与窗之间的说话人编号没有关系。** 每个 10 秒窗里的 speaker#1/#2/#3 是**局部编号**，
   窗 A 的 1 号和窗 B 的 1 号可能不是同一个人。必须在 CAM++ 嵌入 + 全局聚类之后才能统一。
   所以 pyannote 只负责「切」，不负责「认」。
2. **单窗上限：3 人、2 人同时说话。** 5 人会议里同一 10 秒有 4 人抢话会失真，
   但概率极低；真出现时由聚类后处理兜底。

**重叠段怎么喂 ASR（初稿没写的设计决策）**

两个人同时说话的那一段，任何 ASR 都转不好。默认策略：

- 按 powerset 里**概率最高的单人类别**归属给主导说话人；
- 段上标 `overlap=true`，前端可选择用淡色标记或直接忽略；
- **不把重叠段拆成两段分别喂 ASR**——ASR 拿到的都是混音，拆两段只会得到两份垃圾。

### 4.4 嵌入与预处理

新建 `src-tauri/resources/speaker_embedding.py`：

```python
class SpeakerEmbedder:
    """CAM++ 声纹嵌入。与 FireRedVad 共用 fbank 参数，但归一化方式不同。"""

    def embed(self, audio: np.ndarray) -> np.ndarray:  # -> (embed_dim,) float32
```

**嵌入维度不能写死**：中文双语版权重是 192 维，但本机缓存的英文版是 512 维。
一律从 `session.get_outputs()[0].shape` 或实际输出取，参数里只记日志。

#### 最容易做错的一件事：嵌入单位

**嵌入单位必须是「(10 秒窗, 窗内局部说话人)」，不是 pyannote 切出的短段。**

这是 2026-11-24 实测推翻的初稿写法。初稿按段嵌入，而这些段的**中位长度只有 1.65 秒**
（33.5% 不足 1 秒），CAM++ 在上面极不稳定，直接后果是同一人相邻段的嵌入互不相似，
聚类碎成几十个簇（见 §9.6）。

sherpa-onnx 官方的做法是：

1. 取该窗内该说话人的所有**活动帧**；
2. 把这些帧对应的音频**首尾相接**成一个缓冲（中间的空隙直接丢掉）；
3. 对拼接后的音频算**一个**嵌入。

这样每个嵌入至少覆盖近 10 秒有效语音，且不含静音与别人的声音。同一窗里换人也不影响，
因为每个局部说话人是单独拼的。若该窗内该说话人活动帧不足 10 帧（≈0.17s）则跳过。

#### 三条必须守住的实现细节

1. **逐句均值归一化**：fbank 特征减去**本次拼接音频自身**的均值，不用固定 CMVN。
   这是 §2.4 里那条「不做就塌成一团」。
2. **排除重叠帧**：算嵌入时把「该帧有两个及以上说话人」的帧丢掉
   （`labels * (labels.sum(axis=-1, keepdims=True) < 2)`）。
   重叠处的声纹是两人叠加，既不像 A 也不像 B，留着只会污染两个簇。
3. **`snip_edges` 需实测对齐 sherpa-onnx**：不同 fbank 边界处理会让嵌入产生系统性偏移。
   验证方法写进单测（§9.2）：把同一段 3 秒音频分别按 [0:3s] 与 [0.75:3s] 取窗，
   归一化后**余弦相似度应 ≥ 0.90**；不同说话人应 ≤ 0.60。达不到说明预处理没对齐。
4. **会话选项与 VAD 一致**：`intra_op_num_threads` 不要给满、`enable_cpu_mem_arena = False`。
   分离与 ASR 在同一进程里跑，不要让它抢核（这是 `firered_vad.py` 里已有的做法）。

### 4.5 聚类与后处理

**聚类对象**：§4.4 产出的 (窗, 局部说话人) 嵌入。31 分钟录音实测约 **2400 个**，
`O(n²)` 距离矩阵约 23MB（float32），纯 numpy 够用，不需要 sklearn。

**关键提醒：阈值不能照抄 openwhispr。** 它调的是一套不同的嵌入模型与归一化流程，
阈值不可直接迁移。下面这组值来自 2026-11-24 的真实录音校准（§9.6）：

| 参数 | 校准值 | 实测依据 |
|---|---|---|
| `CLUSTER_DISTANCE_CUT` | **0.40**（距离，= 余弦 0.60） | 同一人簇内两两距离 0.22–0.26；两个真人之间 0.48。取其间 |
| `MERGE_CENTROID_COS` | **0.85** | 同一人跨 31 分钟质心余弦 **0.973**；两个真人 **0.683** |
| `PHANTOM_MAX_SHARE` | 0.10 | 沿用，实测有效 |
| `PHANTOM_MAX_MEAN_SECS` | 3.0 | 沿用，实测有效 |
| `COLLAPSE_MAX_SHARE` | 0.93 | 沿用 |
| `MAX_SPEAKERS` | 8 | 沿用 |
| `LONG_AUDIO_THRESHOLD_DELTA` | **暂不启用** | 实测同一人跨 31 分钟自相似 0.973，**没有观察到漂移**，openwhispr 的「随时长放宽」在这条链路上没有依据 |

**两个必须记住的数字对比**：

```
同一人跨 31 分钟：0.973
两个不同的人    ：0.683
openwhispr 的合并阈值 0.5 → 会把这两个真人并成一个（实测在距离 0.55 处确实并了）
```

所以 `MERGE_CENTROID_COS` 用 0.5 是**错的**，必须 ≥0.85。这一条直接对应 §10 那条
「把两个真人合并比多一个幽灵更严重」——0.5 恰好会犯这个错。

**四步后处理**（顺序固定）：

1. **幽灵过滤**：占比 <10% 且平均段长 <3s → 丢弃。实测把 99 个簇砍到 4 个。
   永远不碰最大的两个簇。
2. **同簇合并**：质心余弦 ≥ `MERGE_CENTROID_COS`（0.85）。实测只合并 1 对。
3. **坍塌检测**：最大簇占比 >93% 且其余段均 <2.2s → 判为「两人被并成一个」，放宽阈值重聚类。
4. **短段归并**：<0.4s 或独占一簇但极短 → 并入时间上最近的邻段。

**如果聚类规模超过 1200 个点**（长录音会到几千），不要直接跑 O(n³) 的完整层次聚类：
在至多 1200 个点的随机抽样上跑层次聚类，再把全量点按最近质心归簇、迭代两轮细化质心。
实测 2403 个点走这条路耗时 **秒级**，且相邻阈值之间的簇结构变化是连续的（没有跳变）。

> 注意：**没有做过「抽样 vs 全量」的逐点对照**，因为全量 O(n³) 在 2400 点上跑不动。
> 所以这条只是工程上的合理近似，P0 落地时如果有性能余量，应该在全量上复核一次。

一条原则写在这里，实现时不要违背：

> **把两个真人合并，比多出一个幽灵说话人更严重。**

所以幽灵过滤**永远不碰最大的两个簇**，宁可多留一个假人，也不能把两个人并成一个。
合并阈值同理必须留出足够余量（0.85 而不是 0.5）。

### 4.6 降级路径（必须实现，否则这个功能会拖垮转写成功率）

现在有两个新增模型，降级要分三级，不能一缺就退回整段：

```
读引擎配置
 ├─ 在线引擎（glm-asr / alibaba-asr） → 老路径，UI 标注「当前引擎不支持说话人分离」
 └─ 本地引擎
     ├─ 用户关掉了分离开关                → 老路径（静默，不提示）
     ├─ 时长 > 2 小时                     → 老路径 + UI 明示「超长录音未做分离」
     ├─ 输入是内存字节（无路径）           → 老路径：分离要按 offset 读盘，字节没有位置信息
     ├─ pyannote ✓ + CAM++ ✓  → 完整分离（语音段 + 重叠标记 + 说话人编号）
     ├─ pyannote ✗ + CAM++ ✓  → 降级 A：用 FireRedVAD 语音段直接做嵌入与聚类
     │                          段边界粗糙、丢重叠信息，但「谁在说」仍然可用
     ├─ CAM++ ✗               → 老路径：没有声纹就无从聚类，分段出来也归不了人
     ├─ 分离阶段抛异常         → 记日志，老路径（整段转写，文字照出）
     └─ 逐段 ASR 部分失败      → 该段文字留空并保留时间戳，其余段正常返回
```

**降级 A 值得单独实现**：pyannote 的 5.8MB 是运行时下载，下载失败/被墙的概率不低。
此时用已有的 FireRedVAD 段直接跑 CAM++ + 聚类，虽然抢话处会切错，
但「两三个人轮流说话」的普通会议仍然能给出可用的说话人编号——比完全没有强得多。

**分离是旁路，不是主路。**任何一环坏掉都只退化成「没有说话人标签」或「标签更粗糙」，
不会退化成「转不出文字」。

---

## 5. 数据模型

### 5.1 新增两张表

`src-tauri/src/services/notes_service.rs` 的 `ensure_schema` 里追加：

```sql
-- 分段转写结果。一段音频一次 start/stop = 一行 note_recordings，
-- 一行 note_recordings 对应多行这里的结果段。
CREATE TABLE IF NOT EXISTS note_recording_segments (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    recording_id INTEGER NOT NULL REFERENCES note_recordings(id) ON DELETE CASCADE,
    speaker_id   INTEGER NOT NULL DEFAULT 0,   -- 本次录音内的聚类编号，从 0 起
    start_ms     INTEGER NOT NULL,
    end_ms       INTEGER NOT NULL,
    text         TEXT    NOT NULL DEFAULT '',
    overlap      INTEGER NOT NULL DEFAULT 0,   -- 1 = 这段有多人同时说话
    sort_order   INTEGER NOT NULL DEFAULT 0,
    created_at   INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_note_recording_segments
    ON note_recording_segments(recording_id, sort_order);

-- 说话人显示名。P2 才用；P0/P1 前端按 speaker_id 生成「说话人 N」。
CREATE TABLE IF NOT EXISTS note_recording_speakers (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    recording_id INTEGER NOT NULL REFERENCES note_recordings(id) ON DELETE CASCADE,
    speaker_id   INTEGER NOT NULL,
    label        TEXT    NOT NULL DEFAULT '',
    created_at   INTEGER NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_note_recording_speakers
    ON note_recording_speakers(recording_id, speaker_id);
```

依据现有约定：
- 一律走 `db::ensure_schema`，**不引入版本号/迁移链**（应用未分发，表变了直接删库重建）
- `note_recording_segments` 建表前 `notes_service::ensure_schema` 已建 `note_recordings`，
  外键父表必须先存在
- 删录音段 → 级联删分段结果与说话人名，不留孤儿

### 5.2 `transcript` 字段语义保持不变

`note_recordings.transcript` **仍然是拼接后的纯文本**，只是变成带说话人前缀的多行：

```
说话人 1：今天先过一下上周的进展。
说话人 2：好的，我这边主要是三个点。
```

这样做的原因很实际——摘要链路读的就是 `transcript` 字符串
（`notes.transcript` / `summary_source_hash` 都依赖它），**加前缀就能让摘要模型看到多人对话结构，
而完全不用改摘要代码**。结构化数据在 `note_recording_segments`，两者由同一次事务写入。

### 5.3 与现有事务约定一致

写「历史行 + `transcript` + 分段」必须**同一个 `IMMEDIATE` 事务**，
沿用 `save_recording_transcription_with_connection` 的既有做法扩展一个
`save_recording_transcription_with_segments`，命令层不要「先写分段再写历史」地拼两条事务。

> **实施结论（已落地）**：扩展后不带分段的那条旧入口被删掉了，
> 只留 `save_recording_transcription_with_segments` 一条写路径——两条路径各写一遍
> 「写历史 + 写转录」迟早会分叉，而 `segments` 传空表正好表达「这次没有分段」。
> 重新转写时旧分段先删后插，降级成空分段会把旧记录一起清掉。

---

## 6. 接口与命令

### 6.1 Python 侧

新增三个模块 + 一个 action：

| 文件 | 内容 |
|---|---|
| `src-tauri/resources/speaker_segmentation.py` | `SpeakerSegmenter`：加载 pyannote seg ONNX、10s 滑窗、powerset 解码成片段（含 overlap 标记） |
| `src-tauri/resources/speaker_embedding.py` | `SpeakerEmbedder`：加载 CAM++ ONNX、fbank、逐句归一化、`embed(audio) -> (192,)` |
| `src-tauri/resources/speaker_diarize.py` | `Diarizer`：串起流水线 + 聚类 + 后处理 + 合并同人相邻段；聚类与后处理是纯 numpy，可独立单测 |
| `src-tauri/resources/server_common.py` | `_execute_action` 新增 `transcribe_diarized` |

请求/响应（与现有 IPC 风格一致）：

```json
// → 请求
{ "action": "transcribe_diarized", "audio_path": "...", "options": { "expected_speakers": null } }

// ← 响应
{
  "success": true,
  "text": "说话人 1：...\n说话人 2：...",
  "language": "zh",
  "duration": 1832.4,
  "segmentation": "pyannote",
  "speakers": [{ "speaker_id": 0, "segment_count": 42 }, { "speaker_id": 1, "segment_count": 31 }],
  "segments": [
    { "speaker_id": 0, "start_ms": 1200, "end_ms": 5400, "text": "今天先过一下上周的进展。", "overlap": false },
    { "speaker_id": 1, "start_ms": 5400, "end_ms": 7100, "text": "我先补充一点。", "overlap": false }
  ]
}
```

`segmentation` 取值 `pyannote` / `vad-only`（降级 A）/ `none`，Rust 侧据此决定
`fallback_reason` 的文案，不必猜。

**只有 Qwen3 server 实现**；R2T2 收到该 action 返回
`{"success": false, "error": "当前引擎暂不支持说话人分离"}`，Rust 侧据此降级。
（R2T2 支持放 P3，两者 server 结构相似，但它的流式路径与分离的批处理语义不同，不宜一起做。）

### 6.2 Rust 侧

**拟新增** `src-tauri/src/services/transcription_service.rs` 里的 `transcribe_with_speakers`：

```rust
pub struct SpeakerSegment {
    pub speaker_id: i32,
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
    /// 这段里有多人同时说话。前端可选择显示标记，也可忽略。
    pub overlap: bool,
}

pub struct DiarizedTranscription {
    pub text: String,
    pub language: Option<String>,
    pub duration: Option<f64>,
    pub segments: Vec<SpeakerSegment>,
    /// 降级原因。`Some(..)` 表示这次走的是老路径，UI 可据此提示。
    pub fallback_reason: Option<String>,
}

pub async fn transcribe_with_speakers(
    app: &tauri::AppHandle,
    state: &AppState,
    input: AudioInput<'_>,
    options: TranscribeOptions<'_>,
) -> Result<DiarizedTranscription, AppError>
```

- 拿到与 `transcribe` **同一把锁**（`state.engine.native_asr_owner`），
  因为它同样会用 Python 进程里的 ASR session
- 在线引擎 / 模型缺失 / IPC 报 unsupported → **内部转发给 `transcribe`**，
  把结果包成单段 `segments` + `fallback_reason`
- 这样**命令层只需要换一个函数名**，降级逻辑全部收敛在服务层，命令层不必写 `match`

#### 选路与锁顺序（两处会写错的地方）

**(1) 分离路径与 `WavChunker` 切片路径互斥，不是嵌套。**

这一点必须先说清，否则实现时会绕远。既有的选路在 `plan_transcription`：

```
transcribe()
 ├─ 在线引擎 → dispatch_online
 └─ 本地引擎 → plan_transcription()
      ├─ should_chunk(duration) 且输入是文件 → Plan::Chunked → transcribe_chunked()
      └─ 否则                                → Plan::Whole   → transcribe_whole()
```

`transcribe_chunked` 用 `WavChunker` 逐 30 分钟读盘、逐块转写、`join_transcripts` 拼成一段纯文本。
它**不产出任何分段结构**，所以走这条路必然没有说话人编号。

正确的关系是：**分离是 `plan_transcription` 的另一个前置分支，且自己不切片。**

```
transcribe_with_speakers()
 ├─ 能否分离？
 │    （本地引擎 && 开关开 && 有路径 && 时长 ≤ 2h && CAM++ 在）
 │   ├─ 能 → 分离 IPC（Python 侧流式按 offset 读盘，一次跑完，全局聚类）
 │   └─ 不能 → 转发 transcribe() → plan_transcription() → Chunked / Whole
```

所以 **`plan_transcription`、`transcribe_chunked`、`transcribe_whole` 一行都不用改**。
`transcribe_with_speakers` 是纯新增函数，「超 2 小时不分离」只是它入口条件里的一个 `duration <= 2h`，
超了就直接转发——而 `transcribe` 因为时长 ≥ `CHUNK_THRESHOLD_SECS` 会自动走 `Chunked`，正是期望的行为。

这也顺带解决了初稿里担心的「切片间编号不连续」：**分离路径不走 `WavChunker`，全程只聚类一次，
编号天然全局一致**。不存在「每 30 分钟独立分离」这种中间态——那会是自找的麻烦，不要做。

**(2) 转发给 `transcribe` 时绝对不能先持锁，否则死锁。**

`state.engine.native_asr_owner` 是 `tokio::sync::Mutex`，**不可重入**
（`transcription_service.rs` 的注释已明确写了这一点，`*_locked` 变体存在的唯一理由就是这个）。
`transcribe` 自己会 `lock()`。所以顺序必须是「先判条件，再按需拿锁」：

```rust
// 判条件阶段：不持锁
if let Some(reason) = diarize_unavailable_reason(&state, &input, options.duration_sec) {
    let plain = transcribe(app, state, input, options).await?;   // 它自己拿锁
    return Ok(wrap_single_segment(plain, Some(reason)));
}

// 只在真的要跑分离时才拿锁
let _guard = state.engine.native_asr_owner.lock().await;
funasr_service::transcribe_diarized_locked(state, path, app, opts).await
```

反过来写（先拿锁再判条件）会在降级分支上永久挂起，且因为不报错，排查起来极费时间。
§9.1 的单测要专门锁住这条。

**拟新增**命令（`src-tauri/src/commands/note_transcription.rs`）：

```rust
#[tauri::command]
pub async fn transcribe_note_recording_diarized(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    note_id: i64,
    force: Option<bool>,
) -> Result<notes_service::NoteDetail, AppError>
```

实现上就是把 `TranscribeRun` 的 `transcribe` 里对 `run_asr` 的调用换成 `transcribe_with_speakers`，
其余（`try_begin` 占位、历史行、`reuse_history` 就地更新、`history-updated` 事件）全部复用。
**并发保护仍由后端 `state.note_transcription` 原子占位负责，不靠前端按钮 disabled。**

新增命令必须同步登记 `formal/verification.json`（commands / command_implementation / contracts evidence），
否则 `scripts/check_formal.py` 会失败。

**旧命令 `transcribe_note_recording` 保留不动**：它是历史/上传等调用点的入口，
也是新命令降级后的兜底事实来源。P0 只把笔记详情页的「转写 / 重新转写」按钮改成调新命令，
其余调用点一行不改——这样这次改动的影响面被限制在笔记页。

其余新命令（P2）：

| 命令 | 用途 |
|---|---|
| `list_note_segments(recording_id)` | 读分段（笔记详情渲染用） |
| `rename_note_speaker(recording_id, speaker_id, label)` | 说话人改名 |

### 6.3 模型分发（两个模型、一条渠道）

| 模型 | 体积 | 渠道 | 处理 |
|---|---|---|---|
| FireRedVAD | 2.3MB | 已随包内置 | 不动 |
| pyannote segmentation-3.0 | 5.8MB | **HF 镜像**（`csukuangfj/sherpa-onnx-pyannote-segmentation-3-0`，`model.onnx`，非 gated） | 沿用现有 HF 下载器 |
| CAM++ zh_en advanced | ~27MB | **HF 镜像**（`csukuangfj/speaker-embedding-models`，非 gated） | 沿用现有 HF 下载器 |

**原方案里的「pyannote 只能走 GitHub release tar.bz2」是错的**，已作废。
当时判断「HF 上没有可用的非 gated ONNX 仓库」，实际是 `huggingface.co` 连不上，
只测了正站没测镜像；换成 `hf-mirror.com` 复核后，两个仓库一直都在，而且都是
`sherpa-onnx` 作者自己转的 ONNX，输入签名与运行时代码一致。

结论：**不需要给下载器加 GitHub + tar.bz2 分支**，`download_models.py` 的 HF 路径
原样复用，只是多两条仓库条目。少一条下载链就少一套断点、校验、清理逻辑。

其余约定不变：

- 目标目录与 ASR 模型同级（沿用现有 `paths` 解析）
- 下载完成写 `.light_whisper_complete.json` 清单，沿用现有完整性校验
- **模型不存在时不要报错阻塞**，按 §4.6 降级（两个模型缺任一个都退回整段转写）
- 开关关掉时（`LIGHT_WHISPER_WITH_DIARIZATION=0`）不下载这 33MB

### 6.4 上传音频同样走分离

「上传音频文件」不是笔记录音，但它同样是本地引擎、同样有一段落盘的 WAV，
所以没有理由只让笔记享受分离。两条链路在转写这一步是**同一个入口**：

| 链路 | 命令 | 转写入口 | 分段表 |
|---|---|---|---|
| 笔记录音 | `transcribe_note_recording` | `transcribe_with_speakers` | `note_recording_segments`（外键 `recording_id`） |
| 上传音频 | `transcribe_imported_audio` | `transcribe_with_speakers` | `history_segments`（外键 `history_id`） |

**分段表不复用**：笔记那张的外键指向 `note_recordings`，一个笔记可以有 N 段录音；
上传的历史没有「录音段」这一层，硬套就要造一条假 `note_recordings` 行。
两张表字段一致、语义一致，只是父表不同，这是可接受的重复——比伪造父行干净。

**返回结构**：`transcribe_imported_audio` 从返回 `HistoryRecord` 改成

```rust
ImportedTranscription {
    record: HistoryRecord,     // 历史行本身
    segments: Vec<ImportedSpeakerSegment>,  // camelCase，给前端直接渲染
    fallback_reason: Option<String>,        // 没分离的原因码，None = 真分离了
}
```

不把 `segments` 塞进 `HistoryRecord`：那个结构被历史列表、剪贴板、笔记反查共用，
加一个只在导入页有用的字段，会让每次列表查询都白读一遍分段表。

**为什么返回包装结构而不是「另开一个查询命令」**：正文、分段、降级原因属于同一次
转写。分开取会出现「正文已经是新的、分段还是上一份」的中间态；一起返回就没有这个窗口。
`list_history_speaker_segments` 仍然保留，给「重新打开一条历史」用。

**历史正文本身就带说话人**：分离成功时 Python 返回的 `text` 已经是
「说话人 1：…\n说话人 2：…」的多行文本，直接存进 `transcription_history.text`。
好处是历史页、剪贴板、导出都不用改就能看到说话人；代价是字数统计会把前缀算进去。

**正文编辑与分段的关系**：上传页的识别结果可以就地编辑，但那个编辑**本来就不落库**
（`TranscriptionResult` 在上传页没有接 `onTextChange`），所以不存在「分段与正文不一致」
的持久化问题。界面上的处理是两态切换：默认看「说话人」，要改字就切「正文」。
一旦将来把上传页的编辑接上落库，就必须决定「改正文要不要重排分段」——现在不做这个决定。

**已知限制：存为笔记会丢掉分段视图**。`create_note_from_history` 只把历史正文搬进
`notes.transcript`，不建 `note_recordings` 行，所以笔记侧的分段表没有父行可挂。
用户看到的仍然是带「说话人 1：」前缀的正文，只是失去分段卡片与视图切换。
要修就得给笔记再加一张「笔记级分段表」（外键 `note_id`）或在搬的时候伪造一条
`note_recordings`——前者多一张表，后者把「历史音频」说成「笔记录音」，
两者都不该顺手做。等「上传 → 笔记」这条路的真实使用反馈再定。

### 6.5 与 GPU 空闲卸载的冲突（2026-11-26 实测崩溃）

**现象**：上传 31 分钟音频转写，分离阶段结束时进程直接死。

```
[WARN] 说话人分离失败（说话人分离转写失败: exception: access violation reading 0x…），本次按整段转写
[WARN] FunASR 通信中断，释放进程并准备重启
```

同一台机器更早一次是 `'NoneType' object has no attribute 'run'`——同一个根因的另一种表现。

**根因**：`gpu_idle.GPU_IDLE_ACTIVITY_ACTIONS` 里只登记了 `transcribe`，漏了
`transcribe_diarized`。后果有三层：

1. 分离命令期间**不持** `_gpu_idle_lock`，看门狗可以随时把运行时卸掉；
2. 分离命令**不刷新** `_last_activity_at`，命令结束后下一次 tick 立刻又判成空闲；
3. 用户开着「GPU 空闲卸载」（本机 `engine.json` 是 180 秒）时，分离本身要跑
   ~100 秒/小时音频，于是 180 秒窗口一定会在分离中途到点。

时序（本机日志）：08:42:52 冷启动 → `_last_activity_at` 停在启动那一刻 →
08:44:11 分离开始 → 08:45:52 正好 180 秒，看门狗 `_close_runtime()`
→ 同一瞬间分离结束、第一次 `session.run` 落在已释放的原生句柄上 → access violation。
卸载发生在属性读取**之前**就是 `session is None`（AttributeError，进程活着）；
发生在读取**之后**就是野指针（进程死）。Python 层只能看见前者，后者只能靠不并发避免。

**修复**：

- `transcribe_diarized` 加进 `GPU_IDLE_ACTIVITY_ACTIONS`。它因此也进了
  `GPU_IDLE_LOCKED_ACTIONS`，整条命令持锁、结束刷新活动时间，与 `transcribe` 完全一致。
- `transcribe_diarized` 在开始逐段识别前再确认一次 `initialized` / `session`，
  被卸就当场 `initialize()` 重载（防御历史残留的其它卸载路径）。
- `_run_inference` 读到 `session is None` 时抛「运行时未加载」，不再让它变成
  `'NoneType' object has no attribute 'run'` 这种看不出所以然的错误。

**教训**：以后任何新增的耗时长命令，必须同时登记进 `gpu_idle` 的两个集合。
漏掉的代价不是「慢一点」，而是**崩进程**——而且崩在原生层，Python 的
`except Exception` 只能捞到一句 `access violation`，没有任何栈帧。
回归测试见 `test_gpu_idle.py::test_diarized_transcription_blocks_unload_and_refreshes_activity`
与 `test_qwen3_asr_server.py::test_diarized_transcription_reloads_runtime_unloaded_during_diarization`。

---

## 7. 前端

笔记详情页的转写区从「一个 `<pre>`」变成「按段渲染」：

```
[说话人 1]  00:12  今天先过一下上周的进展。
[说话人 2]  00:21  好的，我这边主要是三个点。
[说话人 1]  00:35  先说第一个。
```

- 说话人徽章用固定色板按 `speaker_id` 取色（保证同一人在同一篇里颜色一致）
- 无分段数据（降级或老数据）→ 渲染现在的纯文本，**不做任何提示**，避免打扰
- 有 `fallback_reason` → 在转写区顶部一行浅色提示，例如「当前引擎不支持说话人分离」
- 录音中的实时字幕不做分离（面对面听写是单人，无价值），只在转写完成后一次性出结果

上传页的结果卡多一个「说话人 / 正文」两态切换：

- **有分段时默认落在「说话人」**：用户跑分离就是为了看这个，让他再点一下是多余动作
- 「正文」保留原来的可编辑 textarea；两个视图切换时头部高度不变（按钮常驻，不按态增删）
- 没有分段时**不渲染切换按钮**：两个视图内容完全一样，多一个按钮只会让人以为点坏了
- 降级按 `fallback_reason` 的 8 个码分别给文案，只有 `disabled` 不提示——
  那是用户自己在设置里关的，再解释一遍属于唠叨
- 复制在「说话人」视图下带上「说话人 N：」前缀，与历史里存的正文一致；
  在「正文」视图下复制用户当前编辑的草稿
- 分段列表固定高度内滚动（`max-height: 320px`）：175 段的录音交给页面滚，
  会把下面的「存为笔记」按钮顶出屏幕

---

## 8. 实施清单

### P0 — 跑通（核心，必须一次做对的是数据模型与降级）

- [x] `speaker_segmentation.py`：pyannote 加载 + 10s 滑窗 + powerset 解码 + overlap 标记
- [x] `speaker_embedding.py`：CAM++ 加载 + fbank + 逐句均值归一化 + `embed()`
- [x] `speaker_diarize.py`：串流水线 + **窗-说话人级嵌入（§4.4，含窗内拼接与重叠帧排除）**
      + 帧级投票 + 聚类 + 四步后处理 + 同人合并
- [x] 聚类阈值用 §4.5 的校准值起步：`CLUSTER_DISTANCE_CUT=0.40`、`MERGE_CENTROID_COS=0.85`
- [x] `server_common.py`：新增 `transcribe_diarized` action（Qwen3 实现，R2T2 返回 unsupported）
- [x] 三条降级路径都实测过（pyannote 缺 / CAM++ 缺 / 分离抛异常）
- [x] 分离与逐段 ASR 的进度消息（复用 `{"type":"progress"}` 无进展窗口）
- [ ] `download_models.py` 拉两个产物：**两个都是非 gated 的 HF 镜像仓库**
      （`csukuangfj/sherpa-onnx-pyannote-segmentation-3-0`、`csukuangfj/speaker-embedding-models`），
      原计划里的「pyannote 走 GitHub release + tar.bz2 分支」作废——当初判定「HF 没有」
      是因为 huggingface.co 连不上，换 hf-mirror 验证后发现仓库一直都在。
      `--with-diarization` / `LIGHT_WHISPER_WITH_DIARIZATION=1` 已接上应用下载流程，
      但**还没在纯净机器上跑过一次真实下载**
- [x] `notes_service`：建两表 + `save_recording_transcription_with_segments`（单事务）
- [x] `transcription_service` 新增 `transcribe_with_speakers`（含全部降级分支；**注意 §6.2 的锁顺序**）
- [x] 设置项 `diarization_enabled`（默认 `true`）+ 超 2 小时的时长硬上限
- [x] 命令：没有单开 `transcribe_note_recording_diarized`，而是让
      `transcribe_note_recording` 直接走 `transcribe_with_speakers`（§6.2 的「命令层只换函数名」），
      另加 `list_note_speaker_segments` 供前端读分段、`set_note_diarization_enabled` 供设置页开关
- [x] 前端分段渲染：笔记侧 `SpeakerTranscript` 按录音分组渲染、6 色轮转、overlap 标记；
      上传侧 `SpeakerSegments` + `TranscriptionResult` 的「说话人 / 正文」两态切换，
      降级文案按 `fallbackReason` 的 8 个码分别给（`disabled` 不提示，用户自己关的）
- [x] **上传音频同样支持分离**（见 §6.4）：`transcribe_imported_audio` 改走
      `transcribe_with_speakers`，历史行与分段在同一个 `IMMEDIATE` 事务里落
      `history_segments`；返回 `{ record, segments, fallbackReason }`，
      另加 `list_history_speaker_segments` 供重新取分段
- [x] 登记 `formal/verification.json`

### P1 — 质量（拿真实录音调，不能靠猜）

- [x] **性能优化**：嵌入窗步长 1s→4s + 分段滑窗位移 1s→2s，实测 386 → **101 秒/小时**
      （31 分钟真实录音仍是 2 人、份额 74.1%/25.7% 不变）；批处理已实测不可行（§9.5）
- [ ] pyannote 的概率阈值与 `min_duration_on/off` 定标（初值先抄 sherpa-onnx 的默认：0.5 / 0.5 / 0.3 / 0.5）
- [ ] **抢话专项**：本次样本重叠帧仅 2.6%，pyannote 的 overlap 检出能力**尚未验证**（§9.6）
- [ ] 重叠段归属策略验证：判给主导说话人 vs 标「[多人同时说话]」留空，哪个更可读
- [ ] 回声/串音专项：用 §9.6 的「带限 + 电平」判定法区分回声副本与第二个人
- [ ] UI 允许填参会人数（默认自动）
- [ ] 坍塌检测触发重聚类的完整回路
- [ ] 3 人以上场景 + 不同麦克风/房间（§9.4 的标注录音）

### P2 — 体验

- [ ] `note_recording_speakers` 落库 + 改名交互
- [ ] 按说话人筛选/跳转
- [ ] 导出（Markdown / SRT 带说话人）

### P3 — 能力上探（可选，需单独评估）

- [ ] R2T2 引擎支持分离
- [ ] 跨笔记声纹档案（`speaker_profiles`）——**涉及隐私，需单独方案**
- [ ] 实时说话人识别（**当前明确不做**）

---

## 9. 验证

### 9.1 纯函数单测（不需要模型，最先写）

`speaker_diarize.py` 里的聚类与后处理全部是纯 numpy 函数，用合成嵌入即可测：

- `cluster_segments`：给两组明显分离的 192 维向量 → 必须聚成 2 簇
- 幽灵过滤：构造一个只有 2 段、占比 3% 的簇 → 必须被丢
- 幽灵过滤：构造一个占比 40% 的簇 → **必须保留**（宁可留假人也不能并真人）
- 坍塌检测：构造最大簇占比 96% + 其余段均 <2s → 必须触发重聚类
- 短段归并：0.2s 的孤立段 → 必须并入时间最近邻

Rust 侧另有两个纯函数单测（`transcription_service` 内，不需要模型也不起子进程）：

- `diarize_unavailable_reason`：五种输入分别返回对应原因（在线引擎 / 关掉开关 / >2h / 无路径 / CAM++ 缺）
- **锁顺序防回归**：构造一个必然降级的场景，用 `tokio::time::timeout` 包住调用，
  超时即判失败。这条专门拦「先拿锁再转发」的写法——那个 bug 不报错，只会静默挂死。

### 9.2 预处理对齐自检（防止嵌入全塌）

同 §4.4 第 3 条，写成可执行测试：

```
同一说话人：不同起点的两个 3s 窗  → cos ≥ 0.90
不同说话人：两个 3s 窗            → cos ≤ 0.60
```

**门槛已经从 0.35 放宽到 0.60**——这是实测校准的结果：本机 CAM++ 两把不同嗓音的
质心余弦就有 0.683（§9.6），拿 0.35 当「不同人」的门槛会把真人判成同一人。

**这一条不过，后面所有调参都是白费**，因为它说明特征提取本身没对齐 CAM++ 的训练条件。

### 9.3 合成端到端

取两个已知说话人的干净音频，各 30 秒，交叉拼接成 60 秒，施加不同增益（模拟离麦距离），
跑完整流水线，检查：
- 恢复出的簇数 == 2
- 每个真值段的 speaker_id 一致（簇内纯度 100%）
- 边界误差 < 0.5s

### 9.4 真实录音抽样

**3–5 段面对面会议**，覆盖：不同麦克风（笔记本内置 / USB 麦 / 会议全向麦）、
不同人数（2 / 3–4 / 5+）、开外放 vs 戴耳机。

人工标注前 2 分钟的真值，统计：

- **DER（diarization error rate）**：漏检 + 虚检 + 说话人错分的总占比，目标 < 20%
- **重叠段检出**：人工标出抢话区间，看 pyannote 的 `overlap` 标记召回与误报。
  这一项单独列，因为它是选 pyannote 的唯一理由——如果它在这个场景下也检不出抢话，
  那第 ② 步的选择就该重新评估
- 幽灵说话人数：目标 0
- **两个真人被合并的次数：必须为 0**（比 DER 更重要的红线）

### 9.5 性能预算（已换成 2026-11-24 本机实测值）

测试机：本开发机，`intra_op_num_threads = 6`，31.2 分钟真实录音（1869s）。

| 环节 | 31 分钟实测 | 折合 1 小时 | 说明 |
|---|---|---|---|
| pyannote 分段 | **62.3s** | **120s** | 1859 个 10s 窗，**33.5ms/窗** |
| CAM++ 嵌入 | **209.8s** | **404s** | 2403 个 (窗,局部说话人)，**87ms/条** |
| 聚类 + 投票 + 建段 | ~18s | ~35s | 只在 1200 点子样本上跑层次聚类 |
| **合计** | **~290s** | **~560s（9.3 分钟）** | 原预算 120s/小时，**实测超 4.7 倍** |

| 项 | 预算 | 实测 |
|---|---|---|
| 模型体积 | +33MB（pyannote 5.8 + CAM++ 27） | 与预算一致；不进前端 `total_dist_bytes` |
| 峰值内存 | ≤ +150MB | 未超：一次只持一个 10s 窗 |
| 端到端（分离 + 转写） | ≤ 现有时长 × 1.4 | **不达标**，分离本身就要 ×0.16 实时，叠加 ASR 后接近 ×2 |

**批处理不能救这个开销——已实测否掉**：

```
batch= 1:  82.4 ms/条
batch= 4: 111.7 ms/条
batch= 8: 134.2 ms/条
batch=16: 123.4 ms/条
```

原因是要 padding 到最长序列，且 CAM++ 的算力在这些短序列上本来就是访存瓶颈。
**不要按「批量推理更快」的常识去优化这一步**，实测是反的。

**真正有效的杠杆只有一个：削减 10s 窗的冗余。**
窗长 10s、步长 1s 意味着同一段音频被嵌入 **10 次**。把嵌入用的步长放到 3s，
工作量降到 1/3，代价是帧级投票从 10 票降到 3.3 票（边界略糙）。
**这是 P1 的优化项，必须先验证边界精度损失再定，不要默认采用。**

其余方向（提高「本窗内该说话人最少活动音频」的门槛、跳过纯语气词的窗）都是小头。

> 那段 124ms/窗 的第三方数据与本机实测的 33.5ms/窗 差 3.7 倍——**换机器必须重测**，
> 低端笔记本（面对面会议大概率跑在这类机器上）会更慢。

### 9.6 实测报告：31 分钟真实面对面录音（2026-11-24）

**样本**：`note-3-1795350200989.wav`，16k / 单声道 / 16bit，1868s（31.2 分钟），
来自本应用自身的笔记录音。环境为项目 `.venv`（Python 3.11、onnxruntime 1.24.1、
kaldi_native_fbank 1.22.3、numpy 1.26.4），即**打包时的真实依赖**。

这一步的目的不是算 DER（没有人工真值），而是**证伪**：把方案里最可能错的假设拿出来跑。

#### 结论一：初稿的「段级嵌入」不成立，已改为 (窗, 局部说话人) 级

按初稿写法（pyannote 切段 → 每段算嵌入 → 聚类）跑出来：

| 指标 | 实测 |
|---|---|
| 切出段数 | 977 段，后处理合并后 531 段 |
| 段长中位 | **1.65s**，33.5% 不足 1s |
| 聚类结果 | 阈值 0.55 → **67 个簇**，其中 63 个是 1-6 段的碎片 |
| 相邻段同簇率 | **56.9%**（随机分配基线 43.2%） |

相邻段同簇率只比随机高 13 个点，说明同一人相邻两段的嵌入互不相似——**嵌入在这批短段上不可用**。
换成 sherpa-onnx 官方的 (窗, 局部说话人) 级嵌入（§4.4）后：

| 指标 | 段级（初稿） | 窗-说话人级（修正后） |
|---|---|---|
| 嵌入单位有效语音 | ~1.65s | **~10s**（窗内拼接） |
| 嵌入数 | 463 | 2403 |
| 相邻段同簇率 | 56.9% | —— （不再是同一粒度） |
| 最终段数 | 531 | **373** |
| 段长中位 | 1.65s | **2.87s** |

**这条修正把「嵌入单位」从段改成了窗内拼接片段，是本轮最重要的一处改动。**

#### 结论二：阈值不能照抄 openwhispr，0.5 会吃掉一个真人

用一段 31 分钟录音做同源校准，三个数字：

```
同一人跨 31 分钟（同簇前半 vs 后半）：质心余弦 0.973
不同人（两个主簇）：                  质心余弦 0.683
簇内两两余弦 0.78 / 0.74；跨簇均值 0.519
```

- **同一人自相似 0.973** → 这条链路上**没有观察到声纹漂移**，openwhispr 的
  「长录音按小时放宽阈值」在本方案里**没有依据**，已从参数表移除。
- **不同人 0.683 < openwhispr 的合并阈值 0.5？** 注意方向：0.683 高于 0.5，
  所以 openwhispr 那条「质心余弦 ≥0.5 就合并」会**把这两个真人并成一个**。
- 实测确认了这个后果：把距离阈值放到 0.55 时，第二大簇（占 21.7%）被吞进最大簇，
  变成单一簇占 83.9%。**这正是「两个真人被合并」，是本方案定义为最严重的失败。**

因此 `MERGE_CENTROID_COS` 从 0.5 提到 **0.85**，`CLUSTER_DISTANCE_CUT` 定在 **0.40**。

#### 结论三：这段录音确实是两个人，不是单人独白

最初看到「幽灵过滤后只剩一个簇」时，曾误判为单人录音。三条独立证据推翻了它：

| 证据 | 数值 | 含义 |
|---|---|---|
| 转移矩阵 | `[[264,29],[28,27]]`，0→1 与 1→0 几乎对称 | 轮流说话的典型形态，不是碎片抖动 |
| 第二簇的段 | 63 段，中位 3.2s，**最长 44.9s**，跨度 44~1676s | 有实质内容的长段，不是噪声 |
| 长时平均频谱 | 两簇**都没有带限**（低频与 8k 高频俱在），中位 RMS −21.8 / −22.3 dBFS | 同一麦克风同一房间的两个嗓音，不是扬声器/电话回声副本 |
| 谱质心 | 2952Hz vs 2193Hz | 音色确有差异 |

> 这一条值得写进文档，因为「回声副本」与「第二个人」在聚类结果上长得一模一样，
> **只有频谱形状与电平能区分**。面对面会议开外放时，遇到「相似度 0.6-0.7 的第二簇」，
> 先按本表判一下再决定是合并还是保留。

#### 最终指标（cut=0.40，幽灵过滤 + 0.85 合并）

| 项 | 实测 |
|---|---|
| 识别出的说话人 | **2 人**（另有 1 个 3 秒的孤立簇，属残留） |
| 说话人 0 | 300 段 / 1104s，占语音 **59.1%**，段长中位 2.66s |
| 说话人 1 | 73 段 / 406s，占语音 **21.7%**，段长中位 3.22s，最长 44.9s |
| 语音归属率 | 81.0% |
| 相邻同人合并后 ASR 调用 | **172 次**，中位 5.3s，均值 9.6s，最长 65.7s |
| 超 Qwen3 单次上限（115s）的段 | **0 次** |
| 重叠帧占比 | 2.6%（三人同时 0%） |

172 次 ASR 调用 / 31 分钟，即约 **5.5 次/分钟**，与「按说话人轮次切」的预期吻合。
最长段 65.7s 没有超过 Qwen3 的 115s 不分块上限，**不需要在段内再二次分块**。

#### 这次实测没有验证到的

必须说清楚，否则容易过度乐观：

- **DER / 边界精度**：没有人工标注真值，无法计算。§9.4 那 3-5 段标注录音仍然必须做。
- **抢话/重叠**：这段录音重叠帧只有 2.6%，**样本本身几乎不含抢话**，
  所以「pyannote 能否切出抢话」这个选它的核心理由，**本次没有验证**。
  需要一段真实的激烈讨论录音。
- **3 人以上**：只有 2 人，`MAX_SPEAKERS=8` 与超员折叠完全没被触发。
- **不同麦克风/房间/是否外放**：只有 1 个样本、1 套设备。
- **AEC 与回声路径簇合并**：本次频谱判定的结论是「不是回声」，但**外放场景仍未测**。

### 9.7 分段粒度校准：合并阈值 1.0 → 2.0，段长上限 115 → 45（2026-11-26）

#### 现象

31 分钟录音的同一段独白被切成 5 张卡片。真实时间轴：

| 时间 | 说话人 | 文本 | 与前段间隙 |
|---|---|---|---|
| 63.06–65.98 | 1 | 好，我们开始吧。开始，老师。 | — |
| 65.79–67.51 | 2 | 说说小时候成长环境。 | 切换 |
| 67.61–69.19 | 1 | 我我小时候。 | 1.63s |
| 70.96–82.43 | 1 | 我印象里，我出生在一个小院子里… | **1.77s** |
| 83.85–86.21 | 1 | 这个院子的中间有一口井。 | **1.42s** |
| 87.36–102.23 | 1 | 然后大概横着的这个房子是仓库… | **1.15s** |

63–108s 是同一个人在说话，中间只有换气停顿。

#### 根因：阈值落在分布的空白区

三层处理各自带阈值，互不联动：

| 阶段 | 参数 | 实情 |
|---|---|---|
| 帧级补洞 | `extract_segments(max_gap_seconds=0.5)`，硬编码默认值，未走 options | 把 ≤0.5s 的帧空隙先接上了 |
| 段级合并 | `adjacent_merge_gap_seconds = 1.0` | **一次都没触发** |
| 短段重分配 | `min_segment_seconds = 0.3` | 只覆盖极端碎片 |

0.5s 以内的小缝已被第一层吃掉，而真人停顿要么 <0.5s（已合并）、要么 ≥1.15s。
**0.5–1.0s 这个区间几乎不存在**，1.0 是个空阈值。

实测 106 对同说话人相邻段：**0 对**间隔 ≤1.0s，最小值 1.16s，中位 1.16s。
分段是 175 段，45% ≤5s、29% ≤3s、13% ≤1s。

音频能量交叉验证（确认不是分离漏标，是真静音）：

- 段内 RMS 中位 0.0791
- 同说话人缝隙 RMS 中位 0.0084（段内的 10.6%）
- 57% 的缝隙能量低于段内 15%

#### 改动

两处，都在 `DiarizeOptions`：

- `adjacent_merge_gap_seconds: 1.0 → 2.0`（治「碎」）
- `max_segment_seconds: 115 → 45`（治「慢」，见下节）

`extract_segments` 的 0.5 与 `min_segment_seconds` 都保持原样。

合并只在同说话人相邻行之间发生，任何显式 speaker 段都会挡住，所以不改说话人归属。
段长上限只影响「一段超长时怎么切」，不改变谁说了什么。

#### 结果：四档对比（同一条 1868s 录音，完整跑到出文本）

| 配置（gap / 上限） | 分段数 | 段长中位 | 最长段 | ASR 耗时 | 端到端 |
|---|---|---|---|---|---|
| 1.0 / 115（改前） | 175 | 5.74s | 56.97s | 59.9s | 107s |
| 2.0 / 115 | 91 | 9.94s | 112.76s | **138.2s** | **200s** |
| **2.0 / 45（采用）** | **106** | **12.03s** | **43.28s** | **73.0s** | **122s** |
| 2.0 / 30 | 123 | 16.34s | 28.48s | 66.1s | 119s |

`≤3s` 段在三个 2.0 档里都是 23（改前 51），`≤1s` 都是 13（改前 23）——
合并逻辑一样，长段切分不影响碎片数，只影响超长段怎么分。

67.61–122.09s 合成一段，截图里的「碎」消失；说话人切换（65.79 说话人 2 →
67.61 说话人 1）完好，没有被吞。

#### 代价：段变长把 ASR 拖慢了 1 倍

ASR 时间 59.9s → **138.2s**（2.0/115），端到端 107s → **200s**。
原因不是段数，是段长。同一音频实测单次识别成本：

| 音频长度 | 中位耗时 | 单位成本 |
|---|---|---|
| 30s | 749 ms | 25.0 ms/s |
| 60s | 3817 ms | 63.6 ms/s |
| 110s | 17743 ms | **161.3 ms/s** |

30→60s 音频只翻倍，耗时翻了 5.1 倍；60→110s 音频 1.83 倍，耗时 4.65 倍。
是明确的超线性（attention 平方 + 4GB 卡上的显存压力）。

而 `max_segment_seconds = 115` 允许单段到 115s：合并后 8 段 >60s、合计 624s，
这 624s 按 161 ms/s 算要 100 秒；切成 30s 的块只需要 15.6 秒。

**结论：`max_segment_seconds` 的 115 是按「不分块上限」定的，不是按成本定的，
在本机是纯亏。**

#### 定 45，不定 30

30s 上限把 ASR 压到 66.1s，只比 45s 的 73.0s 快 **7 秒（-10%）**，
却多出 **17 张卡片（106 → 123，+16%）**。边际收益已经很小，
而每题都更短意味着更多话被从中间截断（`split_long_segments` 是按时长
平均切，不是按停顿切，切点落在句子中间的机会随段数上升）。

所以采用 **45s**：比 115 省 65 秒 ASR（-47%），比 30 只多 7 秒却少 17 段。

实测 `ms/s` 与定长测试不完全一致（45s 档 40.1 ms/s，而 bench 30s 是 25.0），
因为短段的固定开销被摊在单位成本里：实际 106 段中 40% 短于 5 秒，
这些段的每秒成本远高于 30s 长段。选段长上限时要用整体加权值，不能只看 bench。

#### 一个仍然是空阈值的值

`merge_short_segments` 用 `min_segment_seconds = 0.3`，本次 106 段里只有 1 段
低于它，实际也是空转。暂不动：它防的是模型抽风产出 0.1s 碎片，
而不是常规停顿，语义与 merge gap 不同。

---

## 10. 风险与不做的事

### 风险

| 风险 | 影响 | 应对 |
|---|---|---|
| **阈值照抄参考项目，吃掉一个真人** | 两个真人被并成一个，比多一个幽灵严重得多。实测：0.5 的合并阈值会并、0.55 的距离阈值已经并了 | 用 §4.5 的实测校准值（0.40 / 0.85）；**禁止直接套用 openwhispr 的 0.6 / 0.5** |
| 串音导致影子说话人 | 同一人被拆成两个标签，比没有更误导 | pyannote 的重叠检出 + AEC + 回声簇合并；用 §9.6 的「带限 + 电平」判定法区分回声与真人 |
| pyannote 在门外场景失效 | 第 ② 步白加，抢话仍然切不开 | §9.4 把「重叠段检出」单列为验收项。**注意本次样本重叠仅 2.6%，该项仍未验证** |
| 阈值不可通用 | 换麦克风/房间就崩 | 阈值必须来自标注数据；本次只有 1 个样本，**不足以定标所有场景** |
| **性能超预算** | 实测 ~560s/小时（原预算 120s），31 分钟会议要多等约 4 分钟 | 先做 P1 的窗步长优化（预期降到 ~200s/小时）；进度必须可视化；> 2 小时不分离 |
| 长录音耗时 | 用户以为卡死 | 复用无进展窗口 + 进度事件；> 2 小时直接不分离（转发老切片路径），UI 明示原因 |
| 分离失败拖垮转写 | 用户彻底拿不到文字 | §4.6 三级降级，任何异常都退回老路径 |

### 明确不做

- **实时说话人分离**：面对面听写就是一个人说话，没有分离需求
- **系统音频采集**：不是本次范围，且会引入新的设备权限与 AEC 复杂度
- **跨会议声纹档案**：涉及生物特征存储，需单独的隐私方案与用户授权，不做默认开启
- **在线引擎分离**：GLM/DashScope 收不了分段结果，硬做是「多次计费 + 上下文割裂」
- **说话人情绪/角色推断**：从「谁在说」推到「他在扮演什么角色」是另一类问题，不在本方案

---

## 11. 已确定的三个决定

1. **模型版本：`3dspeaker_speech_campplus_sv_zh_en_16k-common_advanced.onnx`**（中英双语版）。
   与 openwhispr 同款，会议里混说英文术语（API、deadline、OKR）时不掉链子。
2. **触发方式：自动 + 可关，默认开。**
   设置项 `diarization_enabled`（默认 `true`）。用户关掉后 `transcribe_with_speakers`
   静默转发老路径，**不弹任何提示**——主动关掉的人不需要被教育。
3. **超过 2 小时：不分离 + UI 明示。**

关于第 3 条的实现边界，核实后确认它**不构成对既有功能的任何改动**：

| 疑问 | 实际情况 |
|---|---|
| 要改 `plan_transcription` 吗 | 不用。分离是它之前的独立分支，见 §6.2「选路与锁顺序」 |
| 要改 `transcribe_chunked` 吗 | 不用。超 2 小时直接转发 `transcribe`，它自己会走 `Chunked` |
| 要改 `WavChunker` 吗 | 不用。分离路径不切片，`WavChunker` 只服务老路径 |
| 会不会出现「切片间编号不连续」 | 不会。分离路径全程只聚类一次，编号全局一致；切片路径压根不产编号 |

真正的代价只有一处：`TranscriptionResult` 要能表达「有段」与「无段」两种形态。
这是**新增字段与前端条件渲染**，且当前所有录音本来就是无段的，因此向后兼容。

> 「要不要用 pyannote」这个问题已经定了：**用**。理由是核实后初稿的反面论据全部不成立，
> 且它能解决的正好是面对面场景的核心难点。修订过程见 §4.2，验收红线见 §9.4。

### 已经验证到什么程度（2026-11-24）

用一段 31 分钟真实录音跑通了全链路（细节见 §9.6），可以确认：

| 事项 | 状态 |
|---|---|
| 链路能跑通、能出说话人编号 | ✅ 已验证，识别出 2 人，172 次 ASR 调用 |
| 嵌入单位必须是「窗-说话人」 | ✅ 已验证（段级嵌入不可用，同簇率仅 56.9%） |
| 聚类阈值 0.40 / 合并 0.85 | ✅ 已校准（单样本） |
| 模型体积、内存 | ✅ 与预算一致 |
| 性能 | ⚠️ 实测 ~560s/小时，**超原预算 4.7 倍**，需 P1 优化 |
| DER / 边界精度 | ❌ 无人工真值 |
| 抢话 / 重叠检出 | ❌ 样本重叠仅 2.6%，未验证 |
| 3 人以上、多种设备、外放 | ❌ 未覆盖 |

**下一步最有价值的两件事**：① 找一段含抢话的真实讨论录音，验证 pyannote 的重叠检出
（这是选它的唯一理由，目前仍未验证）；② 做窗步长优化，把 ~560s/小时 压到 ~200s/小时。

---

## 附：本方案与 openwhispr 的关键差异一览

| 维度 | openwhispr | 本方案 |
|---|---|---|
| 主场景 | 远程会议（系统音频轨） | 面对面会议（单麦克风混音） |
| 分段 | pyannote-segmentation-3.0（**外部二进制** + gated 模型） | FireRedVAD 粗筛 + pyannote seg-3.0（**同进程 ONNX 直跑**，无 token、无二进制） |
| 声纹 | CAM++ ONNX，**JS 手写** Kaldi fbank | CAM++ ONNX，`kaldi-native-fbank`（不用手写） |
| 嵌入单位 | 「窗-局部说话人」+ 窗内音频拼接 | **同**（实测证明段级嵌入不可用，见 §9.6） |
| 聚类 | sherpa-onnx 内置 | 纯 numpy 层次聚类（sklearn 被排除），超 1200 点先抽样 |
| 聚类阈值 | 0.6，长录音放宽至 0.7 | **距离 0.40 + 合并 0.85**（本机实测校准，不可照抄） |
| 说话人档案 | 三张表 + 跨会议识别 | P3 再做（隐私优先） |
| 降级 | 有 | 有，且**服务层收敛**，命令层无感知 |
