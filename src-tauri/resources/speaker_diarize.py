#!/usr/bin/env python3
# -*- coding: utf-8 -*-

"""Offline speaker diarization: segmentation + embedding + clustering.

Pipeline
--------
1. pyannote segmentation-3.0 over 10s windows -> per-frame local speaker labels
2. CAM++ embedding per (window, local speaker), concatenating that speaker's
   active frames into one ~10s buffer (gaps dropped, overlap frames excluded)
3. Global agglomerative clustering of those embeddings
4. Post-processing: phantom filter / centroid merge / collapse detect / short-segment merge
5. Frame-level voting -> segment extraction -> adjacent same-speaker merge

Every threshold below came from a real 31-minute recording on this machine.
They are **not** transferable from openwhispr: its 0.5 merge threshold would
collapse two genuinely different speakers into one (measured: different people
score 0.683 on centroid cosine, same person across 31 minutes scores 0.973).
See docs/neo/speaker-diarization-plan.md section 9.6 before changing anything.

These are not user settings — a number here has no meaning outside a given
model and machine. Two internal channels exist instead:

* `load_options_from_env` — JSON override via ``LIGHT_WHISPER_DIARIZE_OVERRIDES``,
  for field tuning and regression runs without rebuilding the app.
* `adapt_options` — derives the two *cost* knobs (segment ceiling, embedding
  stride) from the ASR backend, VRAM and audio length, because those genuinely
  differ per device while the accuracy knobs do not.
"""

from __future__ import annotations

import json
import os
from dataclasses import dataclass, field, replace
from pathlib import Path
from typing import Mapping

import numpy as np

SAMPLE_RATE = 16_000

# 现场调参用的覆盖入口（见 `load_options_from_env`）。只给开发与回归用，
# 不进用户配置、不参与打包默认值。
DIARIZE_OVERRIDES_ENV = "LIGHT_WHISPER_DIARIZE_OVERRIDES"


@dataclass(frozen=True)
class DiarizeOptions:
    """Calibrated on a real in-person meeting recording; see module docstring."""

    # Cosine distance cut for average linkage: 0.40 means cosine similarity 0.60.
    cluster_distance_cut: float = 0.40
    # Centroid cosine above which two clusters are the same person.
    # Must stay well above the different-person value (0.683) — 0.85 leaves
    # enough margin that a real second speaker is never swallowed.
    merge_centroid_cos: float = 0.85
    phantom_max_share: float = 0.10
    phantom_max_mean_seconds: float = 3.0
    # A cluster made of one short isolated utterance is noise, however long it is.
    isolated_cluster_max_seconds: float = 5.0
    collapse_max_share: float = 0.93
    collapse_other_mean_seconds: float = 2.2
    max_speakers: int = 8
    min_segment_seconds: float = 0.3
    # 帧级补洞：同一簇内相邻活跃帧的空隙 ≤ 这个值就接上。它必须明显小于
    # `adjacent_merge_gap_seconds`，否则段级合并又变成空阈值——1.0 的合并阈值
    # 当初一次都没触发，就是被这里的 0.5 吃掉了大半区间。
    frame_gap_seconds: float = 0.5
    # 帧投票：一个簇在某一帧的平均得分到这里才算「这一帧是他在说」。
    vote_threshold: float = 0.5
    # 一段里超过这个比例的帧被判为重叠语音，界面上给这段加「重叠」标记。
    overlap_frame_share: float = 0.5
    # 嵌入只用「干净帧」：窗内同时活跃的本地说话人少于这个数才算干净。
    # 重叠帧混着两个声音，嵌入向量谁都像、谁都不像。
    embed_frame_max_local_speakers: int = 2
    # Frames of active speech required before a (window, speaker) pair is embedded.
    min_active_frames: int = 10
    # Embed every Nth window instead of all of them. Consecutive 10s windows
    # overlap by 9s, so a stride of 4 removes ~75% of the embedding cost while
    # still sampling the whole recording. Clustering needs coverage, not density;
    # segment boundaries come from the segmenter and are unaffected.
    # Measured: stride 1 -> 2404 embeddings / 386s per hour, stride 4 -> 611 /
    # 185s, with identical speaker shares (74.1% / 25.7%).
    # HARD LIMIT: stride * window_shift must stay below window_samples. The
    # shipped model has 10s windows shifted 2s -> coverage factor 5, so 4 is
    # the largest safe stride. Stride 6 makes sampled windows 12s apart and
    # leaves a 2s hole every 12s; `_vote` sees cover=0 there, never marks those
    # frames active, and ASR silently drops words inside the hole (measured:
    # 79.26-81.98 "第二，这是巩固和发展全" + 81.98-84.03 unsaid "党高度团结统一"
    # on the 58-minute party-class recording). `_embed_windows` clamps this
    # at runtime regardless of how the value was set.
    embed_window_stride: int = 4
    # Above this many embeddings, cluster on a sample then refine on all points.
    subsample_limit: int = 1200
    # Merge adjacent segments of the same speaker separated by a short pause.
    # 1.0 was a dead threshold: extract_segments already bridges gaps up to
    # 0.5s, so real pauses either stay under 0.5s (already merged) or land at
    # 1.15s and beyond. Measured on a 31-minute recording: of 106 adjacent
    # same-speaker pairs, zero were within 1.0s (minimum 1.16s), so the merge
    # never fired and one monologue came out as five cards. 2.0 keeps the
    # 66-pair 1.0-1.5s cluster and the 21-pair 1.5-2.0s cluster together
    # (175 -> 88 segments, median 5.7s -> 9.9s) without touching a single
    # speaker change, because merges only happen between same-speaker rows.
    adjacent_merge_gap_seconds: float = 2.0
    # Hard ceiling for one ASR call; longer utterances are split afterwards.
    # This is a *cost* knob, not a model limit. 115 was chosen as "Qwen3 can
    # take it without chunking", which is true but ruinous: measured on this
    # machine, one inference costs 25.0 ms per audio-second at 30s, 63.6 at
    # 60s and 161.3 at 110s (attention is quadratic, and 4GB of VRAM starts
    # thrashing). With merge_gap at 2.0 the 115s ceiling produced 8 segments
    # over 60s totalling 624s = ~100s of ASR by itself.
    # 45 vs 30 was measured end to end on the same 31-minute recording:
    # 45s -> 106 segments / 73.0s ASR, 30s -> 123 segments / 66.1s ASR.
    # 17 extra cards buy 7 seconds; 45 is the better trade.
    # 45 是「4GB 卡上」的下界，不是通用最优：显存更大时 `adapt_options` 会放到
    # 60，它只在没有设备信息时兜底。
    max_segment_seconds: float = 45.0
    audio_block_seconds: float = 30.0


# ----------------------------------------------------------------------
# 参数覆盖与设备自适应
#
# 这些数没有「用户想要的正确值」，只有「这套模型 + 这台机器上的正确值」。
# 所以不对外暴露成设置项，只留两个内部通道：
#   1. `load_options_from_env` —— 调参与回归时用环境变量临时改，不重编译；
#   2. `adapt_options` —— 按后端/显存/时长推导两个成本参数（段长上限、
#      嵌入步长），因为它们本来就该跟设备走。
# ----------------------------------------------------------------------

# 采样窗不留缝的最大步长。对当前分段模型（10s 窗 / 2s 移位）是 4；
# 调用方拿到真实 segmenter 后会用 `max_safe_embed_stride` 覆盖这个值。
DEFAULT_MAX_EMBED_WINDOW_STRIDE = 4


def max_safe_embed_stride(window_samples: int, window_shift_samples: int) -> int:
    """相邻采样窗之间不留洞的最大步长（留一窗余量）。

    窗长 W、窗起点间隔 S 时，每 stride 个窗取一个会让采样窗起点相隔
    ``stride * S``。一旦它达到 W，两次采样之间就出现没有任何采样窗覆盖的区间：
    `_vote` 的 cover 在那里是 0，帧永远不 active，整段语音落不进任何 ASR 段，
    界面上表现为「吞字」。倍数 5 时返回 4，而不是踩着边界的 5。
    """
    if window_samples <= 0 or window_shift_samples <= 0:
        return 0  # 调用方拿不到模型元数据时不额外钳制
    return max(1, int(window_samples // window_shift_samples) - 1)


# key -> (类型, 下界, 上界)。范围不是随手圈的，每条都对应一种已知的坏结果：
# 阈值越界不会报错，只会静默分错人，所以这里必须拦住。
_OVERRIDE_BOUNDS: dict[str, tuple[str, float, float]] = {
    # 低于 0.20 会把一个人拆成多个；高于 0.70 会把两个人并成一个。
    "cluster_distance_cut": ("float", 0.20, 0.70),
    # 低于异人实测值 0.683 就会吞掉真实第二人，低于 0.70 无意义。
    "merge_centroid_cos": ("float", 0.70, 0.99),
    "phantom_max_share": ("float", 0.0, 0.5),
    "phantom_max_mean_seconds": ("float", 0.5, 30.0),
    "isolated_cluster_max_seconds": ("float", 0.5, 60.0),
    "collapse_max_share": ("float", 0.5, 1.0),
    "collapse_other_mean_seconds": ("float", 0.5, 30.0),
    "max_speakers": ("int", 1, 20),
    "min_segment_seconds": ("float", 0.0, 5.0),
    # 活跃帧不足这么多就不做嵌入：一帧 0.1s，10 帧约 1 秒。
    "min_active_frames": ("int", 1, 200),
    "frame_gap_seconds": ("float", 0.0, 2.0),
    "vote_threshold": ("float", 0.1, 0.9),
    "overlap_frame_share": ("float", 0.1, 1.0),
    "embed_frame_max_local_speakers": ("int", 1, 4),
    "embed_window_stride": ("int", 1, 16),
    "subsample_limit": ("int", 100, 10000),
    # 必须大于 frame_gap_seconds，否则段级合并不起作用（见字段注释）。
    "adjacent_merge_gap_seconds": ("float", 0.0, 10.0),
    "max_segment_seconds": ("float", 10.0, 115.0),
    "audio_block_seconds": ("float", 5.0, 120.0),
}


def _coerce_override(key: str, raw: object) -> float | int:
    if key not in _OVERRIDE_BOUNDS:
        raise ValueError(
            f"未知参数 {key!r}；可用参数：{', '.join(sorted(_OVERRIDE_BOUNDS))}"
        )
    kind, low, high = _OVERRIDE_BOUNDS[key]
    if isinstance(raw, bool) or not isinstance(raw, (int, float)):
        raise ValueError(f"{key} 需要数字，收到 {raw!r}")
    if kind == "int":
        if float(raw) != int(raw):
            raise ValueError(f"{key} 需要整数，收到 {raw!r}")
        value: float | int = int(raw)
    else:
        value = float(raw)
    if not low <= value <= high:
        raise ValueError(f"{key}={value} 超出范围 [{low}, {high}]")
    return value


def load_options_from_env(
    base: DiarizeOptions | None = None,
    environ: Mapping[str, str] | None = None,
) -> tuple[DiarizeOptions, frozenset[str]]:
    """读 `LIGHT_WHISPER_DIARIZE_OVERRIDES`，返回 (生效参数, 被覆盖的键)。

    值是 JSON 对象，例如 `{"adjacent_merge_gap_seconds": 1.5}`。格式错、键名错、
    类型错、越界都抛 `ValueError`（调用方负责记日志并退回默认值）——静默忽略
    会让人以为调了却没生效，比直接报错更难查。
    """
    base = base or DiarizeOptions()
    env = os.environ if environ is None else environ
    raw = env.get(DIARIZE_OVERRIDES_ENV)
    if raw is None or not raw.strip():
        return base, frozenset()

    try:
        payload = json.loads(raw)
    except json.JSONDecodeError as exc:
        raise ValueError(f"{DIARIZE_OVERRIDES_ENV} 不是合法 JSON: {exc}") from exc
    if not isinstance(payload, dict):
        raise ValueError(f"{DIARIZE_OVERRIDES_ENV} 需要 JSON 对象，收到 {type(payload).__name__}")

    updates = {key: _coerce_override(key, value) for key, value in payload.items()}
    options = replace(base, **updates)
    # 单一字段都在范围内、组合起来仍然可以没意义：帧级补洞已经能接上
    # frame_gap_seconds 以内的缝，段级合并阈值低于它就永远不会触发——1.0 的
    # 合并阈值当年就是这么变成空阈值的，所以这条组合约束必须拦住。
    if options.adjacent_merge_gap_seconds <= options.frame_gap_seconds:
        raise ValueError(
            "adjacent_merge_gap_seconds 必须大于 frame_gap_seconds"
            f"（当前 {options.adjacent_merge_gap_seconds} <= {options.frame_gap_seconds}）"
        )
    return options, frozenset(updates)


# 段长上限的候选阶梯。30 是「可读性下限」（再短就碎成卡片），75 是「成本上限」
# （再长 ASR 单次耗时压过收益）。
SEGMENT_CAP_LADDER = (30.0, 45.0, 60.0, 75.0)

# 单次 ASR 调用的成本拟合：ms ≈ 2.203·L² − 96·L + 1646（L 为音频秒数）。
# 三点实测（30s→749ms、60s→3817ms、110s→17743ms）在本机 3050 Ti + CUDA 上
# 的拟合误差 < 1%。**只用于给段长上限判时间预算**，不参与任何结果计算；
# 换机器/换模型要重测，不能拿它当「预计耗时」展示给用户。
_ASR_CALL_COST = (2.203, -96.0, 1646.0)


def predicted_asr_seconds(speech_seconds: float, segment_seconds: float) -> float:
    """按上面的成本模型估一次总识别耗时（仅供内部预算判断）。"""
    if speech_seconds <= 0 or segment_seconds <= 0:
        return 0.0
    quadratic, linear, fixed = _ASR_CALL_COST
    per_call = quadratic * segment_seconds**2 + linear * segment_seconds + fixed
    return max(0.0, speech_seconds / segment_seconds * per_call / 1000.0)


def _device_segment_cap(backend: str, gpu_memory_total_gb: float | None) -> float:
    """设备能承受的段长上限。

    只有 CUDA + 大显存才放宽：45 这个默认值本身就是在 4GB 卡上「显存开始
    thrash」的位置测出来的，显存翻倍后同样的段长不再撞墙，可以少切几刀。
    拿不到显存就按默认，不做猜测。
    """
    if backend != "cuda" or not gpu_memory_total_gb:
        return 45.0
    if gpu_memory_total_gb >= 8.0:
        return 60.0
    return 45.0


def adapt_options(
    base: DiarizeOptions,
    *,
    backend: str,
    gpu_memory_total_gb: float | None,
    duration_seconds: float,
    locked: frozenset[str] = frozenset(),
    max_embed_window_stride: int = DEFAULT_MAX_EMBED_WINDOW_STRIDE,
) -> DiarizeOptions:
    """按「后端 + 显存 + 音频时长」推导两个成本参数。

    只动两个：`embed_window_stride`（分离侧成本）与 `max_segment_seconds`
    （识别侧成本）。二者都是「给定设备就是常数、换设备就该变」的参数，写死在
    dataclass 里等于假设所有人用同一张卡。`locked` 里的键来自环境变量覆盖，
    优先于自动推导——调参时人的判断高于自动推导。

    **步长不是「越粗越省」的成本旋钮**：它受分段模型的窗覆盖倍数硬约束。
    2026-11-27 曾按「长音频放粗到 6/8」推导，58 分钟音频上每 12s 留 2s 洞，
    吞掉「全党高度团结统一」。现在长音频与 CPU 都不再提步长，只做封顶钳制；
    `_embed_windows` 还有一层按真实模型参数的运行时钳制兜底。
    """
    stride = base.embed_window_stride
    if "embed_window_stride" not in locked:
        ceiling = max(1, int(max_embed_window_stride))
        stride = min(stride, ceiling)

    cap = base.max_segment_seconds
    if "max_segment_seconds" not in locked:
        ceiling = _device_segment_cap(backend, gpu_memory_total_gb)
        # 时间预算：多花的时间不超过 60s 固定开销 + 音频时长的 3%。
        # 语音占比按 90% 估（实测 91–97%），宁可估高一点。
        budget = 60.0 + 0.03 * max(0.0, duration_seconds)
        speech_guess = max(0.0, duration_seconds) * 0.9
        cap = SEGMENT_CAP_LADDER[0]
        for candidate in SEGMENT_CAP_LADDER:
            if candidate > ceiling:
                break
            if predicted_asr_seconds(speech_guess, candidate) <= budget:
                cap = candidate

    return replace(base, embed_window_stride=int(stride), max_segment_seconds=float(cap))



@dataclass
class DiarizeSegment:
    speaker_id: int
    start: float
    end: float
    overlap: bool = False

    @property
    def duration(self) -> float:
        return self.end - self.start


@dataclass
class DiarizeResult:
    segments: list[DiarizeSegment]
    speaker_count: int
    duration: float
    stats: dict = field(default_factory=dict)

    def speaker_durations(self) -> dict[int, float]:
        totals: dict[int, float] = {}
        for segment in self.segments:
            totals[segment.speaker_id] = totals.get(segment.speaker_id, 0.0) + segment.duration
        return totals


# ----------------------------------------------------------------------
# Clustering primitives (pure numpy, no sklearn — it is excluded from the
# packaged runtime).
# ----------------------------------------------------------------------


def _centroids(embeddings: np.ndarray, labels: np.ndarray) -> np.ndarray:
    count = int(labels.max()) + 1 if labels.size else 0
    centroids = np.zeros((count, embeddings.shape[1]), dtype=np.float32)
    for index in range(count):
        members = embeddings[labels == index]
        if len(members):
            centroids[index] = members.mean(axis=0)
    norms = np.linalg.norm(centroids, axis=1, keepdims=True)
    return centroids / np.maximum(norms, 1e-9)


def average_linkage(distance: np.ndarray, cut: float) -> np.ndarray:
    """Agglomerative average-linkage clustering stopped at a distance `cut`.

    Returns labels ordered by decreasing cluster size, so label 0 is always the
    dominant speaker. O(n^3) in the worst case but fully vectorized; fine up to
    the subsample limit.
    """
    n = int(distance.shape[0])
    if n == 0:
        return np.zeros(0, dtype=np.int64)
    if n == 1:
        return np.zeros(1, dtype=np.int64)

    d = np.array(distance, dtype=np.float32, copy=True)
    np.fill_diagonal(d, np.inf)
    sizes = np.ones(n, dtype=np.float32)
    members: dict[int, list[int]] = {index: [index] for index in range(n)}

    while True:
        flat = int(np.argmin(d))
        first, second = divmod(flat, n)
        if not np.isfinite(d[first, second]) or d[first, second] >= cut:
            break
        total = sizes[first] + sizes[second]
        merged = (sizes[first] * d[first] + sizes[second] * d[second]) / total
        d[first, :] = merged
        d[:, first] = merged
        d[first, first] = np.inf
        d[second, :] = np.inf
        d[:, second] = np.inf
        sizes[first] = total
        members[first].extend(members.pop(second))

    groups = sorted(members.values(), key=len, reverse=True)
    labels = np.zeros(n, dtype=np.int64)
    for label, group in enumerate(groups):
        for member in group:
            labels[member] = label
    return labels


def _assign_to_centroids(
    embeddings: np.ndarray,
    initial_labels: np.ndarray,
    seed_embeddings: np.ndarray,
    iterations: int = 2,
) -> np.ndarray:
    centroids = _centroids(seed_embeddings, initial_labels)
    labels = initial_labels
    for _ in range(iterations):
        labels = np.argmax(embeddings @ centroids.T, axis=1)
        refined = _centroids(embeddings, labels)
        # Keep empty clusters from vanishing into index 0.
        for index in range(len(centroids)):
            if index < len(refined) and np.any(labels == index):
                centroids[index] = refined[index]
    return labels


def cluster_embeddings(
    embeddings: np.ndarray,
    cut: float,
    subsample_limit: int = 1200,
    seed: int = 0,
) -> np.ndarray:
    """Cluster normalized embeddings; samples first when the set is large."""
    n = int(embeddings.shape[0])
    if n == 0:
        return np.zeros(0, dtype=np.int64)
    distance = 1.0 - embeddings @ embeddings.T
    np.fill_diagonal(distance, 0.0)
    np.maximum(distance, 0.0, out=distance)

    if n <= subsample_limit:
        return average_linkage(distance, cut)

    rng = np.random.default_rng(seed)
    sample = np.sort(rng.choice(n, subsample_limit, replace=False))
    sample_labels = average_linkage(distance[np.ix_(sample, sample)], cut)
    return _assign_to_centroids(embeddings, sample_labels, embeddings[sample])


# ----------------------------------------------------------------------
# Post-processing rules. Each one takes and returns a segment array of shape
# (n, 3) holding [start_seconds, end_seconds, cluster_id].
# ----------------------------------------------------------------------


def drop_phantom_clusters(
    segments: np.ndarray, options: DiarizeOptions
) -> np.ndarray:
    """Discard tiny fragments, but never touch the two largest clusters.

    Merging two real people is a worse failure than showing a phantom speaker,
    so the protection of the top two clusters is unconditional.
    """
    if len(segments) == 0:
        return segments
    durations = segments[:, 1] - segments[:, 0]
    total = float(durations.sum())
    clusters = segments[:, 2].astype(np.int64)
    if total <= 0:
        return segments

    totals: dict[int, float] = {}
    lengths: dict[int, list[float]] = {}
    for cluster, duration in zip(clusters, durations):
        totals[int(cluster)] = totals.get(int(cluster), 0.0) + float(duration)
        lengths.setdefault(int(cluster), []).append(float(duration))
    ranked = sorted(totals, key=lambda key: totals[key], reverse=True)

    keep = np.ones(len(segments), dtype=bool)
    for rank, cluster in enumerate(ranked):
        if rank < 2:
            continue
        share = totals[cluster] / total
        mean_length = float(np.mean(lengths[cluster]))
        if share < options.phantom_max_share and mean_length < options.phantom_max_mean_seconds:
            keep &= clusters != cluster
        # A single ~3s utterance standing alone is noise, not a participant:
        # it survives the mean-length rule above by a hair, yet labels a whole
        # speaker for one sentence.
        elif len(lengths[cluster]) <= 1 and totals[cluster] < options.isolated_cluster_max_seconds:
            keep &= clusters != cluster
    return segments[keep]


def merge_similar_clusters(
    segments: np.ndarray, centroids: np.ndarray, options: DiarizeOptions
) -> tuple[np.ndarray, np.ndarray]:
    """Merge clusters whose centroids are near-identical; repeat until stable."""
    if len(segments) == 0 or len(centroids) <= 1:
        return segments, centroids

    segments = segments.copy()
    centroids = np.asarray(centroids, dtype=np.float32).copy()
    mapping = np.arange(len(centroids))

    while len(centroids) > 1:
        similarity = centroids @ centroids.T
        np.fill_diagonal(similarity, -np.inf)
        flat = int(np.argmax(similarity))
        first, second = divmod(flat, len(centroids))
        if similarity[first, second] < options.merge_centroid_cos:
            break
        segments[segments[:, 2] == mapping[second], 2] = mapping[first]
        centroids[first] = centroids[first] + centroids[second]
        centroids[first] /= max(float(np.linalg.norm(centroids[first])), 1e-9)
        keep = np.ones(len(centroids), dtype=bool)
        keep[second] = False
        centroids = centroids[keep]
        mapping = mapping[keep]

    # Renumber compactly so cluster ids stay 0..k-1.
    unique = sorted(set(segments[:, 2].astype(np.int64)))
    remap = {old: new for new, old in enumerate(unique)}
    segments[:, 2] = [remap[int(value)] for value in segments[:, 2]]
    return segments, centroids


def detect_collapse(segments: np.ndarray, options: DiarizeOptions) -> bool:
    """True when one cluster swallowed everything but a few crumbs."""
    if len(segments) == 0:
        return False
    durations = segments[:, 1] - segments[:, 0]
    total = float(durations.sum())
    if total <= 0:
        return False
    clusters = segments[:, 2].astype(np.int64)
    totals = {int(c): float(durations[clusters == c].sum()) for c in np.unique(clusters)}
    if len(totals) < 2:
        return False
    largest = max(totals, key=lambda key: totals[key])
    if totals[largest] / total <= options.collapse_max_share:
        return False
    others = [float(np.mean(durations[clusters == c])) for c in totals if c != largest]
    return bool(others) and max(others) < options.collapse_other_mean_seconds


def merge_short_segments(
    segments: np.ndarray, options: DiarizeOptions
) -> np.ndarray:
    """Reassign very short segments to the temporally nearest neighbour."""
    if len(segments) == 0:
        return segments
    segments = segments.copy()
    durations = segments[:, 1] - segments[:, 0]
    order = np.argsort(segments[:, 0])
    for index in order:
        if segments[index, 1] - segments[index, 0] >= options.min_segment_seconds:
            continue
        start = segments[index, 0]
        best = -1
        best_gap = np.inf
        for other in order:
            if other == index:
                continue
            gap = min(abs(segments[other, 0] - segments[index, 1]), abs(start - segments[other, 1]))
            if gap < best_gap:
                best_gap = gap
                best = other
        if best >= 0:
            segments[index, 2] = segments[best, 2]
    return segments


def fold_extra_clusters(
    segments: np.ndarray, centroids: np.ndarray, options: DiarizeOptions
) -> tuple[np.ndarray, np.ndarray]:
    """Merge the smallest clusters into their most similar cluster past the cap."""
    if len(segments) == 0 or len(centroids) <= options.max_speakers:
        return segments, centroids
    segments = segments.copy()
    centroids = np.asarray(centroids, dtype=np.float32).copy()
    mapping = np.arange(len(centroids))
    durations = segments[:, 1] - segments[:, 0]
    totals = {
        int(c): float(durations[segments[:, 2].astype(np.int64) == c].sum())
        for c in np.unique(segments[:, 2])
    }

    while len(centroids) > options.max_speakers:
        small = min(totals, key=lambda key: totals[key])
        small_index = int(np.flatnonzero(mapping == small)[0])
        similarity = centroids @ centroids[small_index]
        similarity[small_index] = -np.inf
        target = int(np.argmax(similarity))
        target_id = int(mapping[target])
        segments[segments[:, 2] == small, 2] = target_id
        totals[target_id] = totals.get(target_id, 0.0) + totals.pop(small, 0.0)
        centroids[small_index] = 0.0
        keep = np.ones(len(centroids), dtype=bool)
        keep[small_index] = False
        centroids = centroids[keep]
        mapping = mapping[keep]

    unique = sorted(set(segments[:, 2].astype(np.int64)))
    remap = {old: new for new, old in enumerate(unique)}
    segments[:, 2] = [remap[int(value)] for value in segments[:, 2]]
    return segments, centroids


def _compact(
    segments: np.ndarray, centroids: np.ndarray
) -> tuple[np.ndarray, np.ndarray]:
    """Renumber cluster ids to 0..k-1 (largest first) and reorder centroids.

    Every post-processing rule below assumes contiguous ids that index directly
    into `centroids`; skipping this after dropping clusters silently deletes
    speakers, because `centroids` and segment ids drift apart.
    """
    if len(segments) == 0:
        return segments, centroids
    durations: dict[int, float] = {}
    for start, end, cluster in segments:
        durations[int(cluster)] = durations.get(int(cluster), 0.0) + (end - start)
    order = sorted(durations, key=lambda key: durations[key], reverse=True)
    lookup = {old: new for new, old in enumerate(order)}
    compacted = segments.copy()
    compacted[:, 2] = [lookup[int(value)] for value in compacted[:, 2]]
    return compacted, np.asarray(centroids, dtype=np.float32)[order]


def extract_segments(
    active: np.ndarray,
    frame_shift: float,
    frame_offset: float,
    overlap: np.ndarray | None,
    options: DiarizeOptions,
) -> np.ndarray:
    """Turn per-cluster frame activity into [start, end, cluster] segments."""
    frame_count, cluster_count = active.shape
    # Frame i starts at i * shift + offset; using the frame centre for the end
    # boundary would clip every segment by half a frame.
    def frame_start(index: int) -> float:
        return index * frame_shift + frame_offset

    rows: list[list[float]] = []
    for cluster in range(cluster_count):
        flags = active[:, cluster]
        if not flags.any():
            continue
        edges = np.diff(flags.astype(np.int8))
        starts = list(np.flatnonzero(edges == 1) + 1)
        ends = list(np.flatnonzero(edges == -1) + 1)
        if flags[0]:
            starts.insert(0, 0)
        if flags[-1]:
            ends.append(frame_count)
        raw = [(frame_start(max(s, 0)), frame_start(e)) for s, e in zip(starts, ends)]
        merged: list[list[float]] = []
        for begin, finish in raw:
            if merged and begin - merged[-1][1] <= options.frame_gap_seconds:
                merged[-1][1] = finish
            else:
                merged.append([begin, finish])
        for begin, finish in merged:
            if finish - begin >= options.min_segment_seconds:
                rows.append([begin, finish, float(cluster)])

    if not rows:
        return np.zeros((0, 3), dtype=np.float64)
    segments = np.asarray(rows, dtype=np.float64)
    segments = segments[np.argsort(segments[:, 0])]
    return segments


def merge_adjacent_same_speaker(
    segments: np.ndarray, max_gap: float
) -> np.ndarray:
    """Collapse runs of the same speaker separated by a short pause.

    This is what drops the ASR call count by an order of magnitude: meetings
    are full of one person speaking several sentences in a row.
    """
    if len(segments) == 0:
        return segments
    merged: list[list[float]] = [list(segments[0])]
    for row in segments[1:]:
        previous = merged[-1]
        if int(row[2]) == int(previous[2]) and row[0] - previous[1] <= max_gap:
            previous[1] = max(previous[1], row[1])
        else:
            merged.append(list(row))
    return np.asarray(merged, dtype=np.float64)


def split_long_segments(segments: np.ndarray, max_seconds: float) -> np.ndarray:
    """Split utterances that exceed a single ASR call budget."""
    if len(segments) == 0 or max_seconds <= 0:
        return segments
    rows: list[list[float]] = []
    for start, end, cluster in segments:
        length = end - start
        if length <= max_seconds:
            rows.append([start, end, cluster])
            continue
        pieces = int(np.ceil(length / max_seconds))
        step = length / pieces
        for piece in range(pieces):
            rows.append([start + piece * step, start + (piece + 1) * step, cluster])
    return np.asarray(rows, dtype=np.float64)


# ----------------------------------------------------------------------
# Audio access
# ----------------------------------------------------------------------


class _AudioBlocks:
    """Block-buffered random access to 16 kHz mono PCM.

    Diarization reads many small spans inside each 10s window; without a block
    buffer that becomes thousands of tiny read calls. One block covers several
    windows, so the source is touched ~once per 30 seconds of audio.
    """

    def __init__(self, source, block_seconds: float = 30.0):
        self._array: np.ndarray | None = None
        self._handle = None
        self._block_samples = max(1, int(block_seconds * SAMPLE_RATE))
        self._buffer = np.zeros(0, dtype=np.float32)
        self._buffer_start = 0
        self.length = 0

        if isinstance(source, np.ndarray):
            self._array = np.ascontiguousarray(source, dtype=np.float32).reshape(-1)
            self.length = int(self._array.size)
        else:
            import soundfile as sf

            self._handle = sf.SoundFile(str(source))
            if self._handle.samplerate != SAMPLE_RATE:
                self._handle.close()
                raise ValueError(
                    f"说话人分离要求 {SAMPLE_RATE} Hz 音频，实际 {self._handle.samplerate} Hz"
                )
            self.length = int(len(self._handle))
            self._channels = int(self._handle.channels)

    def read(self, start: int, end: int) -> np.ndarray:
        start = max(0, int(start))
        end = min(int(end), self.length)
        if end <= start:
            return np.zeros(0, dtype=np.float32)
        if self._array is not None:
            return self._array[start:end]
        if start < self._buffer_start or end > self._buffer_start + self._buffer.size:
            # Align to a block boundary and read two blocks: consecutive 10s
            # windows overlap by 9s, so a buffer keyed to the request start would
            # be discarded on every window and re-read the file thousands of times.
            aligned = (start // self._block_samples) * self._block_samples
            self._buffer_start = aligned
            self._handle.seek(aligned)
            block = self._handle.read(
                self._block_samples * 2, dtype="float32", always_2d=True
            )
            self._buffer = np.ascontiguousarray(block.mean(axis=1, dtype=np.float32))
        begin = start - self._buffer_start
        return self._buffer[begin : begin + (end - start)]

    def close(self) -> None:
        if self._handle is not None:
            self._handle.close()
            self._handle = None

    def __enter__(self):
        return self

    def __exit__(self, *exc_info):
        self.close()


# ----------------------------------------------------------------------
# Pipeline
# ----------------------------------------------------------------------


class Diarizer:
    """Ties segmentation, embedding and clustering together."""

    def __init__(self, segmenter, embedder, options: DiarizeOptions | None = None):
        self.segmenter = segmenter
        self.embedder = embedder
        self.options = options or DiarizeOptions()

    def _effective_embed_stride(self, options: DiarizeOptions) -> int:
        """按真实分段模型的窗覆盖倍数钳制步长（与设置来源无关）。

        这是最后一道防线：即使环境变量把步长覆盖成 8，或旧调用方传进一份
        按长音频放粗过的参数，也不允许采样窗之间出现无覆盖的洞。
        """
        window_samples = int(getattr(self.segmenter, "window_samples", 0) or 0)
        shift_samples = int(getattr(self.segmenter, "window_shift_samples", 0) or 0)
        limit = max_safe_embed_stride(window_samples, shift_samples)
        requested = max(1, int(options.embed_window_stride))
        return min(requested, limit) if limit else requested

    def _embed_windows(self, audio: _AudioBlocks, segmentation, progress=None, options=None):
        """One embedding per (window, local speaker) with enough clean speech.

        `options` 必须由 `run` 传进来：嵌入选窗、步长、最小帧数都随本次音频的
        时长变化，读 `self.options` 会悄悄用构造时那份基线值。
        """
        options = options or self.options
        labels = segmentation.window_labels
        window_count, frames_per_window, local_speakers = labels.shape
        receptive_shift = self.segmenter.receptive_shift_samples
        # Overlap frames mix two voices; the vector resembles neither.
        clean = labels * (
            labels.sum(axis=2, keepdims=True) < options.embed_frame_max_local_speakers
        )

        embeddings: list[np.ndarray] = []
        pairs: list[tuple[int, int]] = []
        report_every = max(1, window_count // 20)
        stride = self._effective_embed_stride(options)
        # Always include the final window so the tail is never unsampled.
        targets = list(range(0, window_count, stride))
        if targets and targets[-1] != window_count - 1:
            targets.append(window_count - 1)

        for target_index, window in enumerate(targets):
            offset = int(segmentation.window_offsets[window])
            for speaker in range(local_speakers):
                flags = clean[window, :, speaker]
                if int(flags.sum()) < options.min_active_frames:
                    continue
                indices = np.flatnonzero(flags)
                breaks = np.flatnonzero(np.diff(indices) > 1) + 1
                pieces: list[np.ndarray] = []
                for block in np.split(indices, breaks):
                    if block.size == 0:
                        continue
                    begin = offset + int(block[0]) * receptive_shift
                    finish = offset + int(block[-1] + 1) * receptive_shift
                    piece = audio.read(begin, finish)
                    if piece.size:
                        pieces.append(piece)
                if not pieces:
                    continue
                span = np.concatenate(pieces)
                vector = self.embedder.embed(span)
                if vector is None:
                    continue
                embeddings.append(vector)
                pairs.append((window, speaker))
            if progress is not None and (
                target_index % report_every == 0 or target_index == len(targets) - 1
            ):
                progress(
                    {
                        "phase": "embed",
                        "window": window + 1,
                        "windows": window_count,
                        "fraction": (target_index + 1) / len(targets),
                    }
                )

        if not embeddings:
            return (
                np.zeros((0, 1), dtype=np.float32),
                np.zeros(0, dtype=np.int64),
            )
        return np.asarray(embeddings, dtype=np.float32), np.asarray(pairs, dtype=np.int64)

    def _vote(
        self,
        segmentation,
        pairs: np.ndarray,
        labels: np.ndarray,
        cluster_count: int,
        options=None,
    ) -> tuple[np.ndarray, np.ndarray]:
        """Aggregate per-window local labels into global cluster frame activity."""
        options = options or self.options
        frame_count = segmentation.frame_count
        receptive_shift = self.segmenter.receptive_shift_samples
        votes = np.zeros((frame_count, cluster_count), dtype=np.float32)
        cover = np.zeros(frame_count, dtype=np.float32)

        by_window: dict[int, list[tuple[int, int]]] = {}
        for row, (window, speaker) in enumerate(pairs):
            by_window.setdefault(int(window), []).append((row, int(speaker)))

        frames_per_window = segmentation.frames_per_window
        for window in range(segmentation.window_count):
            # Windows that were not embedded must not inflate the denominator,
            # otherwise every score drops and whole speakers fall below 0.5.
            rows = by_window.get(window)
            if not rows:
                continue
            start = int(
                np.rint(segmentation.window_offsets[window] / receptive_shift)
            )
            end = min(start + frames_per_window, frame_count)
            length = end - start
            if length <= 0:
                continue
            for row, speaker in rows:
                votes[start:end, labels[row]] += segmentation.window_labels[
                    window, :length, speaker
                ]
            cover[start:end] += 1

        scores = votes / np.maximum(cover, 1.0)[:, np.newaxis]
        # 没有任何采样窗覆盖的帧必须判为不活跃。step=6 时这些帧恰好是吞字的
        # 2s 洞；显式写进判据，避免将来把 vote_threshold 调到 0 或浮点误差
        # 让 cover=0 的帧重新混进段里。
        active = (scores >= options.vote_threshold) & (cover > 0)[:, np.newaxis]
        return scores, active

    def run(self, source, *, progress=None, options: DiarizeOptions | None = None) -> DiarizeResult:
        """Run the pipeline.

        `options` 是「本次请求」的参数：段长上限与嵌入步长要按音频时长和当前
        设备推导，不能在构造时定死（同一个 Diarizer 会处理 2 分钟和 2 小时的
        音频）。不传就用构造时的基线值。
        """
        options = options or self.options
        segmentation = self.segmenter.segment(source, progress=progress)
        duration = segmentation.duration_seconds
        stats: dict = {
            "windows": segmentation.window_count,
            "frames": segmentation.frame_count,
            "duration": round(duration, 3),
            # 本次生效的两个成本参数：自适应选了什么必须能从结果里反查，
            # 否则「为什么这次段更碎/更慢」只能靠猜。
            "max_segment_seconds": options.max_segment_seconds,
            # 实际生效的步长可能被模型窗覆盖倍数钳小；同时保留请求值，
            # 否则「为什么这次没有洞」在日志里查不出来。
            "embed_window_stride": self._effective_embed_stride(options),
            "requested_embed_window_stride": int(options.embed_window_stride),
        }
        if segmentation.frame_count == 0:
            return DiarizeResult([], 0, duration, stats)

        audio = _AudioBlocks(source, options.audio_block_seconds)
        try:
            embeddings, pairs = self._embed_windows(
                audio, segmentation, progress, options
            )
        finally:
            audio.close()

        stats["embeddings"] = int(len(embeddings))
        if len(embeddings) == 0:
            return DiarizeResult([], 0, duration, stats)

        cut = options.cluster_distance_cut
        labels = cluster_embeddings(embeddings, cut, options.subsample_limit)
        segments = np.zeros((0, 3), dtype=np.float64)

        # Collapse can only be judged once segments exist, and fixing it means
        # re-clustering with a tighter cut. Bound the retries.
        for _ in range(3):
            _, active = self._vote(
                segmentation, pairs, labels, int(labels.max()) + 1, options
            )
            segments = extract_segments(
                active,
                segmentation.frame_shift_seconds,
                segmentation.frame_offset_seconds,
                segmentation.overlap,
                options,
            )
            if len(segments) == 0:
                break
            # Cluster ids and centroid rows must be rebuilt together on every
            # pass; carrying stale centroids into the next rule deletes speakers.
            centroids = _centroids(embeddings, labels)
            segments, centroids = _compact(
                drop_phantom_clusters(segments, options), centroids
            )
            segments, centroids = merge_similar_clusters(segments, centroids, options)
            segments, centroids = fold_extra_clusters(segments, centroids, options)
            segments = merge_short_segments(segments, options)
            if not detect_collapse(segments, options):
                break
            cut = max(0.20, cut - 0.10)
            labels = cluster_embeddings(embeddings, cut, options.subsample_limit)
            stats["collapse_retries"] = stats.get("collapse_retries", 0) + 1

        stats["cluster_distance_cut"] = round(cut, 3)
        if len(segments) == 0:
            return DiarizeResult([], 0, duration, stats)

        segments = merge_adjacent_same_speaker(segments, options.adjacent_merge_gap_seconds)
        segments = split_long_segments(segments, options.max_segment_seconds)

        # Final renumber by total duration so speaker 1 is the dominant voice.
        durations: dict[int, float] = {}
        for start, end, cluster in segments:
            durations[int(cluster)] = durations.get(int(cluster), 0.0) + (end - start)
        order = sorted(durations, key=lambda key: durations[key], reverse=True)
        renumber = {old: new for new, old in enumerate(order)}

        overlap_flags = self._overlap_flags(segmentation, segments, options)
        items = [
            DiarizeSegment(
                speaker_id=renumber[int(cluster)],
                start=float(min(start, duration)),
                # The last frame's start plus one frame can land past the audio.
                end=float(min(end, duration)),
                overlap=bool(overlap_flags[index]),
            )
            for index, (start, end, cluster) in enumerate(segments)
        ]
        stats["segments"] = len(items)
        stats["speakers"] = len(renumber)
        stats["asr_calls"] = len(items)
        stats["speech_ratio"] = round(
            sum(item.duration for item in items) / duration, 4
        ) if duration > 0 else 0.0
        stats["overlap_ratio"] = round(float(segmentation.overlap.mean()), 4)
        return DiarizeResult(items, len(renumber), duration, stats)

    def _overlap_flags(self, segmentation, segments: np.ndarray, options=None) -> np.ndarray:
        """Mark segments whose span is mostly covered by overlapped frames."""
        if len(segments) == 0:
            return np.zeros(0, dtype=bool)
        options = options or self.options
        shift = segmentation.frame_shift_seconds
        flags = np.zeros(len(segments), dtype=bool)
        for index, (start, end, _) in enumerate(segments):
            begin = max(0, int((start - segmentation.frame_offset_seconds) / shift))
            finish = min(segmentation.frame_count, int((end - segmentation.frame_offset_seconds) / shift) + 1)
            if finish <= begin:
                continue
            flags[index] = bool(
                segmentation.overlap[begin:finish].mean()
                >= options.overlap_frame_share
            )
        return flags
