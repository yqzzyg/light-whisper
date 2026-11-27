#!/usr/bin/env python3
# -*- coding: utf-8 -*-

"""Resident Qwen3-ASR GGUF server with a shared audio.cpp streaming path."""

import os
from pathlib import Path
import time
import traceback
from types import SimpleNamespace

from asr_stream_server import ASRStreamServerMixin
from firered_vad import FireRedVad
from qwen3_native import QwenNativeRuntime, QwenPipelineRuntime
from r2t2_stream import R2T2StreamSession

from server_common import (
    BaseASRServer,
    apply_hf_env_defaults,
    decode_inline_audio,
    setup_rotating_logger,
)

apply_hf_env_defaults()
logger = setup_rotating_logger(__name__, "qwen3_asr_server.log", "Qwen3-ASR服务器")

from hf_cache_utils import QWEN3_ASR_MODELS, find_hf_snapshot_file

QWEN3_VAD_SAMPLE_RATE = 16_000
# Below this, there are too few embeddings for clustering to say anything and
# the plain path is both faster and more accurate.
MIN_DIARIZATION_SECONDS = 30.0


class Qwen3ASRServer(ASRStreamServerMixin, BaseASRServer):
    """Keep one GGUF model and one KV session resident across requests.

    Idle unload, when the user turns it on, closes both and the next
    transcribe calls initialize() again. FireRedVAD stays loaded.
    """

    _stream_display_name = "Qwen3-ASR"

    def __init__(self, engine=None):
        engine = engine or os.environ.get("LIGHT_WHISPER_ASR_ENGINE", "qwen3-asr-0.6b")
        if engine not in QWEN3_ASR_MODELS:
            raise ValueError(f"不支持的 Qwen3-ASR 引擎: {engine}")
        self.model_config = QWEN3_ASR_MODELS[engine]
        super().__init__(engine=engine, logger=logger)
        self.model = None
        self.session = None
        self.stream = None
        self.vad_model = None
        self.backend = "cuda" if self.device == "cuda" else "cpu"
        self._gpu_device_info_cache = None
        self._total_inference_ms = 0.0
        self._total_vad_ms = 0.0
        self._vad_calls = 0
        self._vad_rejected = 0

    def _get_gpu_device_info(self, *, include_memory: bool = False) -> dict:
        # 这个服务端一直带显存总量（响应里要展示），所以忽略调用方的偏好；
        # 但签名必须跟基类一致，否则基类的通用调用方一传参就 TypeError。
        return super()._get_gpu_device_info(include_memory=True)

    def _resolve_model_path(self):
        return find_hf_snapshot_file(
            self.model_config["repo_id"], self.model_config["filename"]
        )

    def _suspend_gpu_runtime(self):
        """Unload the GGUF model and KV session. FireRedVAD stays on CPU."""
        if not self.initialized or self._stream_is_active():
            return False
        vad = self.vad_model
        self._close_runtime()
        self._gpu_suspended = True
        self.vad_model = vad
        self.logger.info("Qwen3-ASR 空闲超时，已卸载 GPU 模型；FireRedVAD 仍留在 CPU")
        return True

    def _close_runtime(self):
        self.initialized = False
        stream, self.stream = self.stream, None
        if stream is not None:
            try:
                stream.close()
            except Exception:
                pass
            finally:
                close = getattr(stream.runtime, "close", None)
                if close is not None:
                    close()
        try:
            self._close_audio_processor()
        finally:
            session, self.session = self.session, None
            model, self.model = self.model, None
            for runtime in (session, model if model is not session else None):
                if runtime is None:
                    continue
                try:
                    runtime.close()
                except Exception:
                    pass

    def _load_runtime(self, model_path):
        self._gpu_device_info_cache = None
        runtime_root = Path(__file__).resolve().with_name("r2t2-native")
        preferred = ["cuda", "cpu"] if self.device == "cuda" else ["cpu"]
        errors = []
        for backend in preferred:
            try:
                with self.stdout_suppressor.suppress():
                    runtime = QwenNativeRuntime(
                        model_path, runtime_root / backend / "audiocpp.dll",
                        backend=backend, dll_directories=(runtime_root.parent,),
                    )
                self.model = self.session = runtime
                self.backend = self.device = backend
                return
            except Exception as exc:
                errors.append(f"{backend}: {exc}")
                self._close_runtime()
                logger.warning("Qwen3-ASR %s 后端加载失败: %s", backend, exc)
        raise RuntimeError("；".join(errors))

    def _warmup_inference(self):
        try:
            import numpy as np

            started = time.perf_counter()
            rng = np.random.default_rng(0)
            audio = rng.standard_normal(16_000).astype(np.float32) * 0.002
            self.vad_model.warmup()
            with self.stdout_suppressor.suppress():
                self.session.run(audio, timestamps="none")
            logger.info(
                "Qwen3-ASR 与 FireRedVAD 预热完成，耗时 %.3f 秒",
                time.perf_counter() - started,
            )
        except Exception as exc:
            logger.warning("Qwen3-ASR 预热失败（首次识别可能偏慢）: %s", exc)

    def _filter_speech(self, audio):
        import numpy as np

        started = time.perf_counter()
        chunks = self.vad_model.speech_timestamps(audio)
        vad_ms = (time.perf_counter() - started) * 1000
        self._vad_calls += 1
        self._total_vad_ms += vad_ms

        if not chunks:
            self._vad_rejected += 1
            return np.empty(0, dtype=np.float32), 0, vad_ms

        start = max(0, int(chunks[0]["start"]))
        end = min(len(audio), int(chunks[-1]["end"]))
        if end <= start:
            self._vad_rejected += 1
            return np.empty(0, dtype=np.float32), 0, vad_ms

        # Preserve pauses between spoken regions. Qwen still receives natural
        # phrase timing; only idle time before/after the utterance is removed.
        return np.ascontiguousarray(audio[start:end]), len(chunks), vad_ms

    def _run_inference(self, audio, *, context="", language=None):
        session = self.session
        if session is None:
            # 运行时已被卸掉时给一句能看懂的错误。真正的防线是持锁不卸载
            # （见 gpu_idle.GPU_IDLE_ACTIVITY_ACTIONS），因为句柄被释放后仍非
            # None 的情况只能靠不并发来避免，Python 层拦不住 access violation。
            raise RuntimeError("ASR 运行时未加载，无法识别")
        with self.stdout_suppressor.suppress():
            return session.run(audio, timestamps="none", context=context, language=language)

    def _transcribe_stream_audio(self, audio, *, context="", language=None):
        # Stream callbacks bypass the offline busy guard. VAD runs once for
        # each preview/final, preserving pauses and skipping silence entirely.
        audio, segments, _ = self._filter_speech(audio)
        if not segments:
            return SimpleNamespace(text="", language=language)
        return self._run_inference(audio, context=context, language=language)

    def _create_preview_runtime(self):
        if self.backend == "cuda":
            return QwenPipelineRuntime(
                self._run_inference, prepare_audio=lambda audio: self._filter_speech(audio)[0]
            )
        # CPU VAD and CPU inference share the native worker so they do not
        # compete for cores while new capture packets arrive.
        return QwenPipelineRuntime(self._transcribe_stream_audio, prepare_audio=lambda audio: audio)

    def initialize(self):
        if self.initialized:
            return {"success": True, "message": "模型已初始化", "engine": self.engine}

        model_path = self._resolve_model_path()
        if not model_path:
            return {
                "success": False,
                "error": f"Qwen3-ASR Q8 模型未下载: {self.model_config['filename']}",
                "type": "models_not_downloaded",
                "engine": self.engine,
            }

        started = time.perf_counter()
        try:
            logger.info("加载 Qwen3-ASR: %s", model_path)
            self._load_runtime(model_path)
            if self.vad_model is None:
                self.vad_model = FireRedVad()
            self._warmup_inference()
            self._load_audio_processor()
            self.stream = R2T2StreamSession(
                self._create_preview_runtime(),
                audio_processor=getattr(self, "_audio_processor", None),
            )
            self.initialized = True
            self._gpu_suspended = False
            elapsed = time.perf_counter() - started
            logger.info("Qwen3-ASR 初始化完成，后端: %s，耗时 %.3f 秒", self.backend, elapsed)
            return {
                "success": True,
                "message": f"Qwen3-ASR 初始化成功，耗时: {elapsed:.2f}秒",
                "model_loaded": True,
                "engine": self.engine,
                "backend": self.backend,
                "native_streaming": self.stream is not None,
                **self._get_gpu_device_info(),
            }
        except ImportError as exc:
            self._close_runtime()
            self.vad_model = None
            logger.error("Qwen3-ASR 依赖加载失败: %s", exc)
            logger.error(traceback.format_exc())
            return {
                "success": False,
                "error": f"Qwen3-ASR 依赖加载失败: {exc}",
                "type": "import_error",
                "engine": self.engine,
            }
        except Exception as exc:
            self._close_runtime()
            self.vad_model = None
            logger.error("Qwen3-ASR 初始化失败: %s", exc)
            logger.error(traceback.format_exc())
            return {
                "success": False,
                "error": f"Qwen3-ASR 初始化失败: {exc}",
                "type": "init_error",
                "engine": self.engine,
            }

    def _load_audio(self, audio_path, audio_base64, audio_format, sample_rate):
        import numpy as np

        if audio_base64:
            audio, duration = decode_inline_audio(audio_base64, audio_format, sample_rate)
            if not isinstance(audio, np.ndarray):
                raise ValueError("Qwen3-ASR 内存输入仅支持 PCM")
            source_rate = sample_rate or 16_000
            audio = self._resample(audio, source_rate)
            return np.ascontiguousarray(audio, dtype=np.float32), duration, "memory"

        if not audio_path or not os.path.exists(audio_path):
            raise FileNotFoundError(f"音频文件不存在: {audio_path}")
        audio, source_rate = self._read_mono_audio(audio_path)
        audio = self._resample(audio, source_rate)
        return np.ascontiguousarray(audio), len(audio) / 16_000.0, "path"

    def transcribe_audio(
        self,
        audio_path,
        options=None,
        hot_words=None,
        audio_base64=None,
        audio_format=None,
        sample_rate=None,
    ):
        if not self.initialized:
            init_result = self.initialize()
            if not init_result["success"]:
                return init_result
        if self._stream_is_active():
            return self._stream_busy_response()

        input_mode = "memory" if audio_base64 else "path"
        try:
            options = options if isinstance(options, dict) else {}
            context = self._compose_context(
                options.get("context", ""),
                hot_words if hot_words is not None else options.get("hot_words", []),
            )
            language = self._normalize_language(options.get("language"))
            audio, duration, input_mode = self._load_audio(
                audio_path, audio_base64, audio_format, sample_rate
            )
            self.total_audio_duration += duration
            if duration < 0.5:
                return {
                    "success": True,
                    "text": "",
                    "duration": duration,
                    "engine": self.engine,
                    "input_mode": input_mode,
                }

            processor = getattr(self, "_audio_processor", None)
            if processor is not None:
                audio = processor.enhance(audio)
            self.report_progress("vad_start", duration=round(duration, 3))
            audio, vad_segments, vad_ms = self._filter_speech(audio)
            speech_duration = len(audio) / float(QWEN3_VAD_SAMPLE_RATE)
            if not vad_segments:
                return {
                    "success": True,
                    "text": "",
                    "raw_text": "",
                    "duration": duration,
                    "speech_duration": 0.0,
                    "language": "unknown",
                    "engine": self.engine,
                    "model_type": self.engine,
                    "backend": self.backend,
                    "input_mode": input_mode,
                    "vad_segments": 0,
                    "vad_ms": round(vad_ms, 3),
                    "inference_ms": 0.0,
                }

            self.report_progress(
                "asr_start", speech_duration=round(speech_duration, 3), vad_segments=vad_segments
            )
            started = time.perf_counter()
            result = self._run_inference(audio, context=context, language=language)
            inference_ms = (time.perf_counter() - started) * 1000
            self.report_progress("asr_done", inference_ms=round(inference_ms, 3))
            self._total_inference_ms += inference_ms
            self.transcription_count += 1

            text = result.text.strip()
            language = result.language or "unknown"
            self._maybe_cleanup(duration)
            return {
                "success": True,
                "text": text,
                "raw_text": text,
                "confidence": 0.0,
                "duration": duration,
                "speech_duration": round(speech_duration, 3),
                "language": language,
                "engine": self.engine,
                "model_type": self.engine,
                "backend": self.backend,
                "input_mode": input_mode,
                "vad_segments": vad_segments,
                "vad_ms": round(vad_ms, 3),
                "inference_ms": round(inference_ms, 3),
            }
        except Exception as exc:
            logger.error("Qwen3-ASR 转录失败: %s", exc)
            logger.error(traceback.format_exc())
            return {
                "success": False,
                "error": f"音频转录失败: {exc}",
                "type": "transcription_error",
                "input_mode": input_mode,
            }

    def _diarizer(self):
        """Build the diarization pipeline once; models stay resident afterwards.

        Returns None when either model is missing — callers must degrade to the
        plain transcription path instead of failing the request.
        """
        cached = getattr(self, "_diarizer_cache", None)
        if cached is not None:
            return cached

        from speaker_diarize import Diarizer
        from speaker_embedding import SpeakerEmbedder, SpeakerEmbeddingOptions
        from speaker_models import find_embedding_model, find_segmentation_model
        from speaker_segmentation import SpeakerSegmenter, SpeakerSegmentationOptions

        segmentation_path = find_segmentation_model()
        embedding_path = find_embedding_model()
        if segmentation_path is None or embedding_path is None:
            return None

        # Diarization runs on CPU while the ASR session is loaded; cap the
        # threads so it does not starve the recognition call that follows.
        threads = max(1, min(4, (os.cpu_count() or 4) // 2))
        segmenter = SpeakerSegmenter(
            segmentation_path, SpeakerSegmentationOptions(num_threads=threads)
        )
        embedder = SpeakerEmbedder(
            embedding_path, SpeakerEmbeddingOptions(num_threads=threads)
        )
        diarizer = Diarizer(segmenter, embedder, self._diarize_base_options()[0])
        self._diarizer_cache = diarizer
        logger.info("说话人分离模型加载完成（分段 + 声纹）")
        return diarizer

    def _diarize_base_options(self):
        """基线参数 + 被环境变量覆盖的键；解析失败退回默认并记 ERROR。

        读一次就缓存：环境变量在一个进程生命周期里不会变，而每次转写都重读
        会让「日志里到底是哪套参数」变得不可信。
        """
        cached = getattr(self, "_diarize_base_cache", None)
        if cached is not None:
            return cached

        from speaker_diarize import DiarizeOptions, load_options_from_env

        try:
            options, locked = load_options_from_env()
        except ValueError as exc:
            # 不抛给上层：覆盖写错不该让转写失败，但它必须显眼。
            logger.error("说话人分离参数覆盖无效，改用默认值: %s", exc)
            options, locked = DiarizeOptions(), frozenset()
        if locked:
            logger.info(
                "说话人分离参数被环境变量覆盖: %s",
                ", ".join(sorted(f"{key}={getattr(options, key)}" for key in locked)),
            )
        self._diarize_base_cache = (options, locked)
        return self._diarize_base_cache

    def _diarize_options_for(self, duration_seconds: float):
        """按本次音频时长与当前设备推导本次要用的参数，并把生效值写进日志。"""
        from speaker_diarize import (
            DEFAULT_MAX_EMBED_WINDOW_STRIDE,
            adapt_options,
            max_safe_embed_stride,
        )

        base, locked = self._diarize_base_options()
        # `transcribe_diarized` 先建 diarizer 再算本次参数，所以这里拿得到
        # 真实的分段模型元数据。步长上限必须按模型的窗长/移位算，不能写死：
        # 那正是 stride 6/8 留下 2s 洞、吞掉「全党高度团结统一」的原因。
        diarizer = getattr(self, "_diarizer_cache", None)
        segmenter = getattr(diarizer, "segmenter", None)
        max_stride = DEFAULT_MAX_EMBED_WINDOW_STRIDE
        if segmenter is not None:
            derived = max_safe_embed_stride(
                int(getattr(segmenter, "window_samples", 0) or 0),
                int(getattr(segmenter, "window_shift_samples", 0) or 0),
            )
            if derived:
                max_stride = derived
        options = adapt_options(
            base,
            backend=self.backend,
            gpu_memory_total_gb=self._get_gpu_device_info(include_memory=True).get(
                "gpu_memory_total"
            ),
            duration_seconds=duration_seconds,
            locked=locked,
            max_embed_window_stride=max_stride,
        )
        logger.info(
            "说话人分离本次参数: 段长上限=%ss 嵌入步长=%s(上限%s) 合并间隔=%ss 帧补洞=%ss",
            options.max_segment_seconds,
            options.embed_window_stride,
            max_stride,
            options.adjacent_merge_gap_seconds,
            options.frame_gap_seconds,
        )
        return options

    def _diarization_progress(self, payload):
        phase = payload.get("phase")
        fraction = round(float(payload.get("fraction", 0.0)), 3)
        if phase == "segment":
            self.report_progress("diarize_segment", fraction=fraction)
        elif phase == "embed":
            self.report_progress("diarize_embed", fraction=fraction)

    def transcribe_diarized(self, audio_path, options=None, hot_words=None):
        """Segment by speaker, then transcribe each segment in order.

        Every failure returns a typed error so the Rust layer can fall back to
        the plain path without guessing; diarization is a side channel and must
        never cost the user their transcript.
        """
        import numpy as np
        import soundfile as sf

        if not self.initialized:
            init_result = self.initialize()
            if not init_result["success"]:
                return init_result
        if self._stream_is_active():
            return self._stream_busy_response()

        options = options if isinstance(options, dict) else {}
        if not audio_path or not os.path.exists(audio_path):
            return {
                "success": False,
                "error": f"音频文件不存在: {audio_path}",
                "type": "input_error",
            }

        try:
            with sf.SoundFile(audio_path) as handle:
                source_rate = handle.samplerate
                duration = len(handle) / float(source_rate)
        except Exception as exc:
            logger.error("读取音频失败: %s", exc)
            return {
                "success": False,
                "error": f"无法读取音频: {exc}",
                "type": "input_error",
            }

        if source_rate != QWEN3_VAD_SAMPLE_RATE:
            return {
                "success": False,
                "error": f"说话人分离要求 {QWEN3_VAD_SAMPLE_RATE} Hz 音频，实际 {source_rate} Hz",
                "type": "unsupported_sample_rate",
            }
        if duration < MIN_DIARIZATION_SECONDS:
            return {
                "success": False,
                "error": "录音过短，无需说话人分离",
                "type": "audio_too_short",
            }

        try:
            diarizer = self._diarizer()
        except Exception as exc:
            logger.error("说话人分离模型加载失败: %s", exc)
            logger.error(traceback.format_exc())
            return {
                "success": False,
                "error": f"说话人分离模型加载失败: {exc}",
                "type": "diarization_error",
            }
        if diarizer is None:
            from speaker_models import missing_models

            return {
                "success": False,
                "error": "说话人分离模型未下载",
                "type": "diarization_models_missing",
                "missing": missing_models(),
            }

        try:
            self.report_progress("diarize_segment", duration=round(duration, 3))
            started = time.perf_counter()
            result = diarizer.run(
                audio_path,
                progress=self._diarization_progress,
                options=self._diarize_options_for(duration),
            )
            diarize_ms = (time.perf_counter() - started) * 1000
            if not result.segments:
                return {
                    "success": True,
                    "text": "",
                    "raw_text": "",
                    "duration": round(duration, 3),
                    "speech_duration": 0.0,
                    "language": "unknown",
                    "engine": self.engine,
                    "model_type": self.engine,
                    "backend": self.backend,
                    "input_mode": "path",
                    "segments": [],
                    "speakers": [],
                    "fallback_reason": "no_speech",
                }

            context = self._compose_context(
                options.get("context", ""),
                hot_words if hot_words is not None else options.get("hot_words", []),
            )
            language_option = self._normalize_language(options.get("language"))

            # 分离阶段可能比空闲超时还长（一小时的录音约 100 秒，两小时更久）。
            # 现在 transcribe_diarized 会像 transcribe 一样整段持锁，看门狗进不来；
            # 但卸模型是另一条线程的动作，历史上就是在这里被卸掉后打到野指针的，
            # 所以开始逐段识别前再确认一次运行时就位。
            if not self.initialized or self.session is None:
                if self.session is None:
                    # 已 initialized 但运行时丢了：initialize() 会因为 initialized
                    # 为真直接返回，必须先清干净再重载。
                    self._close_runtime()
                reload_result = self.initialize()
                if not reload_result.get("success"):
                    return reload_result

            self.report_progress(
                "diarize_asr_start", segments=len(result.segments), speakers=result.speaker_count
            )
            segments_out = []
            detected_language = None
            inference_ms = 0.0
            total = len(result.segments)
            with sf.SoundFile(audio_path) as handle:
                for index, segment in enumerate(result.segments):
                    begin = max(0, int(segment.start * source_rate))
                    finish = min(int(segment.end * source_rate), len(handle))
                    if finish - begin < int(0.2 * source_rate):
                        continue
                    handle.seek(begin)
                    chunk = handle.read(finish - begin, dtype="float32", always_2d=True)
                    pcm = np.ascontiguousarray(chunk.mean(axis=1, dtype=np.float32))
                    segment_started = time.perf_counter()
                    output = self._run_inference(
                        pcm, context=context, language=language_option
                    )
                    inference_ms += (time.perf_counter() - segment_started) * 1000
                    text = (output.text or "").strip()
                    if text and detected_language is None:
                        detected_language = output.language or "unknown"
                    segments_out.append(
                        {
                            "speaker_id": int(segment.speaker_id),
                            "start_ms": int(round(segment.start * 1000)),
                            "end_ms": int(round(segment.end * 1000)),
                            "text": text,
                            "overlap": bool(segment.overlap),
                        }
                    )
                    # One progress line per segment would flood the pipe on a
                    # one-hour recording; every fifth is plenty for a heartbeat.
                    if index % 5 == 0 or index == total - 1:
                        self.report_progress(
                            "diarize_asr", done=index + 1, total=total
                        )

            labels = {
                speaker: f"说话人 {speaker + 1}" for speaker in range(result.speaker_count)
            }
            lines = [
                f"{labels.get(item['speaker_id'], item['speaker_id'])}：{item['text']}"
                for item in segments_out
                if item["text"]
            ]
            text = "\n".join(lines)

            durations: dict[int, float] = {}
            for item in segments_out:
                durations[item["speaker_id"]] = durations.get(item["speaker_id"], 0.0) + (
                    item["end_ms"] - item["start_ms"]
                ) / 1000.0
            speakers_out = [
                {
                    "speaker_id": speaker,
                    "segment_count": sum(
                        1 for item in segments_out if item["speaker_id"] == speaker
                    ),
                    "duration": round(value, 3),
                }
                for speaker, value in sorted(durations.items())
            ]

            speech_duration = sum(segment.duration for segment in result.segments)
            self.report_progress(
                "diarize_done",
                speakers=result.speaker_count,
                diarize_ms=round(diarize_ms, 3),
            )
            self._total_inference_ms += inference_ms
            self.transcription_count += 1
            return {
                "success": True,
                "text": text,
                "raw_text": text,
                "duration": round(duration, 3),
                "speech_duration": round(speech_duration, 3),
                "language": detected_language or "unknown",
                "engine": self.engine,
                "model_type": self.engine,
                "backend": self.backend,
                "input_mode": "path",
                "segments": segments_out,
                "speakers": speakers_out,
                "diarization": {
                    "segmentation": "pyannote-segmentation-3.0",
                    "embedding": "cam++",
                    "speaker_count": result.speaker_count,
                    "diarize_ms": round(diarize_ms, 1),
                    "inference_ms": round(inference_ms, 1),
                    **result.stats,
                },
            }
        except Exception as exc:
            logger.error("说话人分离转写失败: %s", exc)
            logger.error(traceback.format_exc())
            return {
                "success": False,
                "error": f"说话人分离转写失败: {exc}",
                "type": "diarization_error",
            }

    def get_performance_stats(self):
        return {
            "transcription_count": self.transcription_count,
            "total_audio_duration": round(self.total_audio_duration, 2),
            "average_inference_ms": round(
                self._total_inference_ms / max(1, self.transcription_count), 3
            ),
            "average_vad_ms": round(self._total_vad_ms / max(1, self._vad_calls), 3),
            "vad_calls": self._vad_calls,
            "vad_rejected": self._vad_rejected,
            "initialized": self.initialized,
            "engine": self.engine,
            "backend": self.backend,
            "native_streaming": self.initialized and self.stream is not None,
            "speculative_decoding": False,
            "models_loaded": {
                "asr": self.model is not None,
                "vad": self.vad_model is not None,
                "punc": True,
            },
        }

    def check_status(self):
        model_loaded = self.model is not None and self.session is not None
        runtime_root = Path(__file__).resolve().with_name("r2t2-native")
        installed = model_loaded or any(
            (runtime_root / backend / "audiocpp.dll").is_file()
            for backend in ("cuda", "cpu")
        )
        return self._with_gpu_suspend_flag({
            "success": True,
            "installed": installed,
            "initialized": self.initialized,
            "engine": self.engine,
            "backend": self.backend,
            "model_loaded": model_loaded,
            "native_streaming": self.initialized and model_loaded and self.stream is not None,
            "models": {
                "asr": model_loaded,
                "vad": self.vad_model is not None,
                "punc": True,
            },
            **self._get_gpu_device_info(),
        })


if __name__ == "__main__":
    Qwen3ASRServer().run()
