#!/usr/bin/env python3
# -*- coding: utf-8 -*-

"""CAM++ speaker embedding runtime for offline speaker diarization.

The model is 3D-Speaker's CAM++ (zh/en bilingual), run through onnxruntime.
Input is a Kaldi-compatible 80-dim log-mel fbank at 16 kHz, matching the
conventions used by sherpa-onnx; see `SpeakerEmbeddingOptions` for the knobs
that must not be changed without re-calibrating the clustering thresholds.

Two things are easy to get wrong here and both silently destroy accuracy:

1. **Utterance-level mean normalization** is mandatory. Subtracting the fixed
   CMVN used by FireRedVAD instead of the utterance's own mean collapses the
   embeddings into a narrow, non-discriminative cone.
2. **The embedding unit must be a ~10s span of one speaker**, not the short
   segments a segmenter produces. Segments under ~1.5s give embeddings that
   are not even similar to themselves (see the plan doc, section 9.6).
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

import numpy as np

SAMPLE_RATE = 16_000
# Shortest span we are willing to embed. Below this CAM++ was trained on
# nothing comparable and the vectors become unstable.
MIN_AUDIO_SAMPLES = SAMPLE_RATE // 2
MODEL_FILENAME = "3dspeaker_speech_campplus_sv_zh_en_16k-common_advanced.onnx"


@dataclass(frozen=True)
class SpeakerEmbeddingOptions:
    """fbank parameters must stay identical to the calibrated pipeline."""

    num_mel_bins: int = 80
    frame_length_ms: float = 25.0
    frame_shift_ms: float = 10.0
    low_freq: float = 20.0
    # Negative means "Nyquist minus this value", i.e. 7600 Hz at 16 kHz.
    high_freq: float = -400.0
    num_threads: int = 1


class SpeakerEmbedder:
    """Compute L2-normalized CAM++ embeddings from 16 kHz mono PCM."""

    def __init__(
        self,
        model_path,
        options: SpeakerEmbeddingOptions | None = None,
    ):
        import kaldi_native_fbank as knf
        import onnxruntime as ort

        self.options = options or SpeakerEmbeddingOptions()
        self.model_path = Path(model_path)
        if not self.model_path.is_file():
            raise FileNotFoundError(f"CAM++ 声纹模型不存在: {self.model_path}")

        fbank_options = knf.FbankOptions()
        fbank_options.frame_opts.samp_freq = SAMPLE_RATE
        fbank_options.frame_opts.frame_length_ms = self.options.frame_length_ms
        fbank_options.frame_opts.frame_shift_ms = self.options.frame_shift_ms
        fbank_options.frame_opts.dither = 0
        fbank_options.frame_opts.snip_edges = True
        fbank_options.frame_opts.window_type = "povey"
        fbank_options.mel_opts.num_bins = self.options.num_mel_bins
        fbank_options.mel_opts.low_freq = self.options.low_freq
        fbank_options.mel_opts.high_freq = self.options.high_freq
        fbank_options.use_energy = False
        self._knf = knf
        self._fbank_options = fbank_options

        session_options = ort.SessionOptions()
        session_options.inter_op_num_threads = 1
        session_options.intra_op_num_threads = self.options.num_threads
        session_options.enable_cpu_mem_arena = False
        session_options.log_severity_level = 4
        self._session = ort.InferenceSession(
            str(self.model_path),
            providers=["CPUExecutionProvider"],
            sess_options=session_options,
        )
        self._input_name = self._session.get_inputs()[0].name
        output_shape = self._session.get_outputs()[0].shape
        # Some CAM++ exports report a symbolic last dim; fall back to probing.
        self.dimension = (
            int(output_shape[-1]) if isinstance(output_shape[-1], int) else 0
        )

    def _fbank(self, audio: np.ndarray) -> np.ndarray | None:
        samples = np.ascontiguousarray(audio, dtype=np.float32).reshape(-1)
        if samples.size < MIN_AUDIO_SAMPLES:
            return None
        fbank = self._knf.OnlineFbank(self._fbank_options)
        fbank.accept_waveform(SAMPLE_RATE, samples)
        fbank.input_finished()
        frames = fbank.num_frames_ready
        if frames < 10:
            return None
        features = np.stack(
            [fbank.get_frame(index) for index in range(frames)]
        ).astype(np.float32)
        # Per-utterance mean normalization (3D-Speaker FBank mean_nor=True).
        # Deliberately not the fixed CMVN that firered_vad.py applies.
        return features - features.mean(axis=0, keepdims=True)

    def embed(self, audio: np.ndarray) -> np.ndarray | None:
        """Return an L2-normalized embedding, or None if audio is unusable."""
        features = self._fbank(audio)
        if features is None:
            return None
        output = self._session.run(None, {self._input_name: features[np.newaxis, :, :]})[0]
        vector = np.asarray(output, dtype=np.float32).reshape(-1)
        if not self.dimension:
            self.dimension = int(vector.size)
        norm = float(np.linalg.norm(vector))
        if not np.isfinite(norm) or norm <= 0.0:
            return None
        return vector / norm

    def warmup(self) -> None:
        self.embed(np.zeros(SAMPLE_RATE, dtype=np.float32))
