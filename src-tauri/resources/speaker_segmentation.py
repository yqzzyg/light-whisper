#!/usr/bin/env python3
# -*- coding: utf-8 -*-

"""pyannote segmentation-3.0 runtime for offline speaker diarization.

The model emits a 7-class powerset per frame (3 local speakers -> 7 subsets),
which is the only output form that can express "two people talking at once".
Everything about *which* global speaker a frame belongs to is decided later by
clustering CAM++ embeddings; this module only answers "how many, and where".

Audio is streamed from disk in blocks. A one-hour recording is ~115 MB of PCM,
so the segmenter must never hold the whole thing in memory.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

import numpy as np

SAMPLE_RATE = 16_000
MODEL_FILENAME = "sherpa-onnx-pyannote-segmentation-3-0.onnx"


@dataclass(frozen=True)
class SpeakerSegmentationOptions:
    """Knobs for the sliding window pass.

    `window_shift_seconds` is the main performance lever: the official 0.1 *
    window means every 1s of audio re-runs a full 10s window. Measured on this
    machine, 2.0s halves the window count and takes 1h of audio from 115s to
    ~58s, while frame-level resolution is unchanged (frames are still 16.9ms;
    only the vote count per frame drops from 10 to 5). Pass None for the
    official 1.0s behaviour when boundary precision matters more than time.
    """

    window_shift_seconds: float | None = 2.0
    frame_threshold: float = 0.5
    num_threads: int = 2
    # Block size used to stream PCM from disk.
    block_seconds: float = 60.0


@dataclass
class SegmentationResult:
    """Frame-level speaker activity over the whole recording."""

    # (window_count, frames_per_window, num_local_speakers) bool — the hard
    # powerset label of every frame *within its own window*. Speaker indices
    # here are local to a window and carry no global meaning.
    window_labels: np.ndarray
    # (window_count,) sample offset of each window in the source audio.
    window_offsets: np.ndarray
    # (frame_count, num_local_speakers) float32 — fraction of covering windows
    # that considered the local speaker active in this frame.
    frame_scores: np.ndarray
    # True where two or more local speakers are active in the same frame.
    overlap: np.ndarray
    frame_shift_seconds: float
    frame_offset_seconds: float
    duration_seconds: float
    num_local_speakers: int

    @property
    def frame_count(self) -> int:
        return int(self.frame_scores.shape[0])

    @property
    def window_count(self) -> int:
        return int(self.window_labels.shape[0])

    @property
    def frames_per_window(self) -> int:
        return int(self.window_labels.shape[1])

    def frame_times(self) -> np.ndarray:
        """Centre time of every frame, in seconds."""
        indices = np.arange(self.frame_count, dtype=np.float64)
        return indices * self.frame_shift_seconds + self.frame_offset_seconds


def _powerset_matrix(num_classes: int, num_speakers: int) -> np.ndarray:
    """Map each powerset class to the set of speakers it represents.

    Class 0 is silence. The remaining classes follow the pyannote ordering:
    singletons first, then pairs, then (rarely used) larger subsets.
    """
    matrix = np.zeros((num_classes, num_speakers), dtype=np.float32)
    index = 1
    for speaker in range(num_speakers):
        if index >= num_classes:
            return matrix
        matrix[index, speaker] = 1.0
        index += 1
    for first in range(num_speakers):
        for second in range(first + 1, num_speakers):
            if index >= num_classes:
                return matrix
            matrix[index, first] = 1.0
            matrix[index, second] = 1.0
            index += 1
    return matrix


def _softmax(logits: np.ndarray) -> np.ndarray:
    shifted = logits - logits.max(axis=-1, keepdims=True)
    exp = np.exp(shifted)
    return exp / exp.sum(axis=-1, keepdims=True)


class SpeakerSegmenter:
    """Run the pyannote powerset segmenter over 16 kHz mono audio."""

    def __init__(
        self,
        model_path,
        options: SpeakerSegmentationOptions | None = None,
    ):
        import onnxruntime as ort

        self.options = options or SpeakerSegmentationOptions()
        self.model_path = Path(model_path)
        if not self.model_path.is_file():
            raise FileNotFoundError(f"说话人分段模型不存在: {self.model_path}")

        session_options = ort.SessionOptions()
        session_options.inter_op_num_threads = 1
        session_options.intra_op_num_threads = max(1, self.options.num_threads)
        session_options.enable_cpu_mem_arena = False
        session_options.log_severity_level = 4
        self._session = ort.InferenceSession(
            str(self.model_path),
            providers=["CPUExecutionProvider"],
            sess_options=session_options,
        )
        self._input_name = self._session.get_inputs()[0].name

        meta = self._session.get_modelmeta().custom_metadata_map or {}
        try:
            self.window_samples = int(meta["window_size"])
            self.receptive_shift_samples = int(meta["receptive_field_shift"])
            self.receptive_size_samples = int(meta["receptive_field_size"])
            self.num_local_speakers = int(meta["num_speakers"])
            self.num_classes = int(meta["num_classes"])
        except KeyError as exc:
            raise ValueError(
                f"分段模型缺少必要元数据 {exc}，不是 pyannote segmentation-3.0 导出"
            ) from exc

        shift = self.options.window_shift_seconds
        if shift is None:
            # Official behaviour: 10% of the window, i.e. 1s for a 10s window.
            self.window_shift_samples = int(round(0.1 * self.window_samples))
        else:
            self.window_shift_samples = max(1, int(round(float(shift) * SAMPLE_RATE)))
        if self.window_shift_samples > self.window_samples:
            raise ValueError("窗步长不能大于窗长")

        self._powerset = _powerset_matrix(self.num_classes, self.num_local_speakers)
        # Samples advanced per output frame.
        self.frame_shift_seconds = self.receptive_shift_samples / SAMPLE_RATE
        self.frame_offset_seconds = self.receptive_size_samples / SAMPLE_RATE * 0.5

    # -- audio access ---------------------------------------------------

    def _window_offsets(self, total_samples: int) -> list[int]:
        if total_samples < self.window_samples:
            return [0]
        offsets = list(
            range(0, total_samples - self.window_samples + 1, self.window_shift_samples)
        )
        # Always analyse the tail so the last seconds are not silently dropped.
        tail = total_samples - self.window_samples
        if offsets[-1] != tail:
            offsets.append(tail)
        return offsets

    def _read_block(self, handle, start_sample: int, frames: int) -> np.ndarray:
        handle.seek(start_sample)
        block = handle.read(frames, dtype="float32", always_2d=True)
        if block.size == 0:
            return np.zeros(0, dtype=np.float32)
        return np.ascontiguousarray(block.mean(axis=1, dtype=np.float32))

    def segment(self, source, *, progress=None) -> SegmentationResult:
        """Analyse `source` (path or float32 array) and return frame scores."""
        import soundfile as sf

        total_samples = 0
        handle = None
        audio: np.ndarray | None = None
        if isinstance(source, np.ndarray):
            audio = np.ascontiguousarray(source, dtype=np.float32).reshape(-1)
            total_samples = int(audio.size)
        else:
            handle = sf.SoundFile(str(source))
            if handle.samplerate != SAMPLE_RATE:
                raise ValueError(
                    f"分段要求 {SAMPLE_RATE} Hz 音频，实际 {handle.samplerate} Hz"
                )
            total_samples = int(len(handle))

        duration = total_samples / SAMPLE_RATE
        empty_labels = np.zeros(
            (0, 0, self.num_local_speakers), dtype=bool
        )
        if total_samples == 0:
            return SegmentationResult(
                window_labels=empty_labels,
                window_offsets=np.zeros(0, dtype=np.int64),
                frame_scores=np.zeros((0, self.num_local_speakers), dtype=np.float32),
                overlap=np.zeros(0, dtype=bool),
                frame_shift_seconds=self.frame_shift_seconds,
                frame_offset_seconds=self.frame_offset_seconds,
                duration_seconds=0.0,
                num_local_speakers=self.num_local_speakers,
            )

        try:
            offsets = self._window_offsets(total_samples)
            # Global frame index each window starts at.
            starts = np.rint(
                np.asarray(offsets, dtype=np.float64) / self.receptive_shift_samples
            ).astype(np.int64)
            frames_per_window = 0
            all_labels: list[np.ndarray] = []
            all_probs: list[np.ndarray] = []
            block_frames = max(self.window_samples, int(self.options.block_seconds * SAMPLE_RATE))
            block = np.zeros(0, dtype=np.float32)
            block_start = 0
            report_every = max(1, len(offsets) // 20)

            for index, offset in enumerate(offsets):
                if audio is not None:
                    window = audio[offset : offset + self.window_samples]
                else:
                    if offset < block_start or offset + self.window_samples > block_start + block.size:
                        block_start = offset
                        block = self._read_block(handle, block_start, block_frames)
                    begin = offset - block_start
                    window = block[begin : begin + self.window_samples]
                if window.size < self.window_samples:
                    window = np.pad(window, (0, self.window_samples - window.size))
                logits = np.asarray(
                    self._session.run(
                        None, {self._input_name: window[np.newaxis, np.newaxis, :]}
                    )[0],
                    dtype=np.float32,
                ).reshape(-1, self.num_classes)
                all_labels.append(self._powerset[np.argmax(logits, axis=-1)] > 0.5)
                all_probs.append(_softmax(logits) @ self._powerset)
                frames_per_window = len(all_labels[-1])
                if progress is not None and (index % report_every == 0 or index == len(offsets) - 1):
                    progress(
                        {
                            "phase": "segment",
                            "window": index + 1,
                            "windows": len(offsets),
                            "fraction": (index + 1) / len(offsets),
                        }
                    )
        finally:
            if handle is not None:
                handle.close()

        window_labels = np.stack(all_labels)
        frame_count = int(starts[-1] + frames_per_window)
        votes = np.zeros((frame_count, self.num_local_speakers), dtype=np.float32)
        probs = np.zeros((frame_count, self.num_local_speakers), dtype=np.float32)
        cover = np.zeros(frame_count, dtype=np.float32)
        for index in range(len(offsets)):
            start = int(starts[index])
            end = min(start + frames_per_window, frame_count)
            length = end - start
            if length <= 0:
                continue
            votes[start:end] += window_labels[index, :length]
            probs[start:end] += all_probs[index][:length]
            cover[start:end] += 1
        safe_cover = np.maximum(cover, 1.0)[:, np.newaxis]
        # Shared windows vote; a speaker active in more than half of them wins.
        frame_scores = votes / safe_cover
        # Overlap comes from probabilities, not votes: where two people talk at
        # once both classes fire, which averaged votes tend to round away.
        overlap = (probs / safe_cover >= self.options.frame_threshold).sum(axis=1) >= 2
        return SegmentationResult(
            window_labels=window_labels,
            window_offsets=np.asarray(offsets, dtype=np.int64),
            frame_scores=frame_scores,
            overlap=overlap,
            frame_shift_seconds=self.frame_shift_seconds,
            frame_offset_seconds=self.frame_offset_seconds,
            duration_seconds=duration,
            num_local_speakers=self.num_local_speakers,
        )

    def warmup(self) -> None:
        self.segment(np.zeros(self.window_samples, dtype=np.float32))
