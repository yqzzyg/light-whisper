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
"""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path

import numpy as np

SAMPLE_RATE = 16_000


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
    # Frames of active speech required before a (window, speaker) pair is embedded.
    min_active_frames: int = 10
    # Embed every Nth window instead of all of them. Consecutive 10s windows
    # overlap by 9s, so a stride of 4 removes ~75% of the embedding cost while
    # still sampling the whole recording. Clustering needs coverage, not density;
    # segment boundaries come from the segmenter and are unaffected.
    # Measured: stride 1 -> 2404 embeddings / 386s per hour, stride 4 -> 611 /
    # 185s, with identical speaker shares (74.1% / 25.7%).
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
    max_segment_seconds: float = 45.0
    audio_block_seconds: float = 30.0


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
    max_gap_seconds: float = 0.5,
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
            if merged and begin - merged[-1][1] <= max_gap_seconds:
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

    def _embed_windows(self, audio: _AudioBlocks, segmentation, progress=None):
        """One embedding per (window, local speaker) with enough clean speech."""
        labels = segmentation.window_labels
        window_count, frames_per_window, local_speakers = labels.shape
        receptive_shift = self.segmenter.receptive_shift_samples
        # Overlap frames mix two voices; the vector resembles neither.
        clean = labels * (labels.sum(axis=2, keepdims=True) < 2)

        embeddings: list[np.ndarray] = []
        pairs: list[tuple[int, int]] = []
        report_every = max(1, window_count // 20)
        stride = max(1, int(self.options.embed_window_stride))
        # Always include the final window so the tail is never unsampled.
        targets = list(range(0, window_count, stride))
        if targets and targets[-1] != window_count - 1:
            targets.append(window_count - 1)

        for target_index, window in enumerate(targets):
            offset = int(segmentation.window_offsets[window])
            for speaker in range(local_speakers):
                flags = clean[window, :, speaker]
                if int(flags.sum()) < self.options.min_active_frames:
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
        self, segmentation, pairs: np.ndarray, labels: np.ndarray, cluster_count: int
    ) -> tuple[np.ndarray, np.ndarray]:
        """Aggregate per-window local labels into global cluster frame activity."""
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
        return scores, scores >= 0.5

    def run(self, source, *, progress=None) -> DiarizeResult:
        options = self.options
        segmentation = self.segmenter.segment(source, progress=progress)
        duration = segmentation.duration_seconds
        stats: dict = {
            "windows": segmentation.window_count,
            "frames": segmentation.frame_count,
            "duration": round(duration, 3),
        }
        if segmentation.frame_count == 0:
            return DiarizeResult([], 0, duration, stats)

        audio = _AudioBlocks(source, options.audio_block_seconds)
        try:
            embeddings, pairs = self._embed_windows(audio, segmentation, progress)
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
            _, active = self._vote(segmentation, pairs, labels, int(labels.max()) + 1)
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

        overlap_flags = self._overlap_flags(segmentation, segments)
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

    @staticmethod
    def _overlap_flags(segmentation, segments: np.ndarray) -> np.ndarray:
        """Mark segments whose span is mostly covered by overlapped frames."""
        if len(segments) == 0:
            return np.zeros(0, dtype=bool)
        shift = segmentation.frame_shift_seconds
        flags = np.zeros(len(segments), dtype=bool)
        for index, (start, end, _) in enumerate(segments):
            begin = max(0, int((start - segmentation.frame_offset_seconds) / shift))
            finish = min(segmentation.frame_count, int((end - segmentation.frame_offset_seconds) / shift) + 1)
            if finish <= begin:
                continue
            flags[index] = bool(segmentation.overlap[begin:finish].mean() >= 0.5)
        return flags
