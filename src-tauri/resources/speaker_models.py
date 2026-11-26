#!/usr/bin/env python3
# -*- coding: utf-8 -*-

"""Locate the two diarization models.

Both live in ordinary (non-gated) HuggingFace repos that mirror the sherpa-onnx
release assets, so they resolve through the same HF cache as the ASR models and
the same download path in `download_models.py`.

An earlier revision of the plan claimed pyannote had no usable HF repo and would
need a GitHub-release tar.bz2 branch. That was wrong: the check had been done
against huggingface.co, which was unreachable at the time. The mirror hosts it,
so the extra download path was never needed.

Both lookups return None when the file is absent; callers must degrade to the
plain transcription path rather than fail the request.
"""

from __future__ import annotations

from hf_cache_utils import find_hf_snapshot_file

SEGMENTATION_REPO_ID = "csukuangfj/sherpa-onnx-pyannote-segmentation-3-0"
SEGMENTATION_FILENAME = "model.onnx"

EMBEDDING_REPO_ID = "csukuangfj/speaker-embedding-models"
EMBEDDING_FILENAME = "3dspeaker_speech_campplus_sv_zh_en_16k-common_advanced.onnx"


def find_segmentation_model() -> str | None:
    return find_hf_snapshot_file(SEGMENTATION_REPO_ID, SEGMENTATION_FILENAME)


def find_embedding_model() -> str | None:
    return find_hf_snapshot_file(EMBEDDING_REPO_ID, EMBEDDING_FILENAME)


def missing_models() -> list[str]:
    """Filenames the caller still needs to download, if any."""
    missing = []
    if find_segmentation_model() is None:
        missing.append(SEGMENTATION_FILENAME)
    if find_embedding_model() is None:
        missing.append(EMBEDDING_FILENAME)
    return missing
