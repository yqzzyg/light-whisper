import os
import sys
import unittest
from dataclasses import fields, replace

import numpy as np

sys.path.insert(0, os.path.dirname(__file__))

from speaker_diarize import (
    DIARIZE_OVERRIDES_ENV,
    DiarizeOptions,
    Diarizer,
    _OVERRIDE_BOUNDS,
    _compact,
    adapt_options,
    average_linkage,
    cluster_embeddings,
    detect_collapse,
    drop_phantom_clusters,
    extract_segments,
    load_options_from_env,
    max_safe_embed_stride,
    merge_adjacent_same_speaker,
    merge_short_segments,
    merge_similar_clusters,
    split_long_segments,
)
from speaker_segmentation import _powerset_matrix, _softmax


def _normalized(rows):
    matrix = np.asarray(rows, dtype=np.float32)
    return matrix / np.linalg.norm(matrix, axis=1, keepdims=True)


class _FakeSegmenter:
    receptive_shift_samples = 1600
    # 与真实 pyannote 导出一致：10s 窗 / 2s 移位 -> 安全步长 4。
    window_samples = 160_000
    window_shift_samples = 32_000


class _FakeEmbedder:
    def __init__(self):
        self.calls = 0

    def embed(self, span):
        self.calls += 1
        return np.array([1.0, 0.0], dtype=np.float32)


class _FakeSegmentation:
    """只够 `_embed_windows` 用：窗数、每窗帧数、一个本地说话人。"""

    def __init__(self, windows=8, frames=20):
        self.window_labels = np.ones((windows, frames, 1), dtype=np.float32)
        self.window_offsets = np.arange(windows) * 160_000


class _FakeAudio:
    def read(self, start, end):
        length = max(0, int(end) - int(start))
        return np.ones(length, dtype=np.float32)


class AverageLinkageTests(unittest.TestCase):
    def test_splits_two_well_separated_groups(self):
        embeddings = _normalized([[1, 0], [0.98, 0.02], [0, 1], [0.02, 0.98]])
        distance = 1.0 - embeddings @ embeddings.T

        labels = average_linkage(distance, 0.5)

        self.assertEqual(len(set(labels.tolist())), 2)
        self.assertEqual(labels[0], labels[1])
        self.assertEqual(labels[2], labels[3])
        self.assertNotEqual(labels[0], labels[2])

    def test_merges_everything_when_cut_exceeds_all_distances(self):
        embeddings = _normalized([[1, 0], [0.98, 0.02], [0, 1], [0.02, 0.98]])
        distance = 1.0 - embeddings @ embeddings.T

        labels = average_linkage(distance, 2.0)

        self.assertEqual(len(set(labels.tolist())), 1)

    def test_handles_empty_and_single_input(self):
        self.assertEqual(average_linkage(np.zeros((0, 0)), 0.5).size, 0)
        self.assertEqual(average_linkage(np.zeros((1, 1)), 0.5).tolist(), [0])

    def test_labels_ordered_by_cluster_size(self):
        embeddings = _normalized([[1, 0]] * 3 + [[0, 1]])
        labels = cluster_embeddings(embeddings, 0.5)

        # The three-point group must be label 0.
        self.assertEqual(labels[:3].tolist(), [0, 0, 0])
        self.assertEqual(int(labels[3]), 1)


class PhantomClusterTests(unittest.TestCase):
    def setUp(self):
        self.options = DiarizeOptions()

    def test_drops_tiny_short_cluster(self):
        # Third cluster is the phantom; the top two are protected by design.
        segments = np.array(
            [[0.0, 10.0, 0], [10.0, 20.0, 0], [20.0, 30.0, 1], [30.0, 31.0, 2]]
        )

        kept = drop_phantom_clusters(segments, self.options)

        self.assertEqual(set(kept[:, 2].astype(int).tolist()), {0, 1})

    def test_keeps_substantial_second_cluster(self):
        segments = np.array([[0.0, 6.0, 0], [6.0, 10.0, 1]])

        kept = drop_phantom_clusters(segments, self.options)

        self.assertEqual(set(kept[:, 2].astype(int).tolist()), {0, 1})

    def test_never_drops_two_largest_clusters(self):
        # Both clusters are short and small, but they are all we have.
        segments = np.array([[0.0, 1.0, 0], [2.0, 3.0, 1]])

        kept = drop_phantom_clusters(segments, self.options)

        self.assertEqual(len(kept), 2)

    def test_drops_lone_short_utterance_cluster(self):
        # A single 3s outlier survives the mean-length rule by a hair, and is
        # exactly the leftover that used to appear as "speaker 3" at 0.2%.
        segments = np.array(
            [[0.0, 20.0, 0], [20.0, 30.0, 1], [30.0, 33.0, 2]]
        )

        kept = drop_phantom_clusters(segments, self.options)

        self.assertEqual(set(kept[:, 2].astype(int).tolist()), {0, 1})

    def test_keeps_lone_long_utterance_cluster(self):
        # Someone who only says one thing, but says it for 8 seconds.
        segments = np.array(
            [[0.0, 20.0, 0], [20.0, 30.0, 1], [30.0, 38.0, 2]]
        )

        kept = drop_phantom_clusters(segments, self.options)

        self.assertEqual(set(kept[:, 2].astype(int).tolist()), {0, 1, 2})


class CollapseDetectionTests(unittest.TestCase):
    def setUp(self):
        self.options = DiarizeOptions()

    def test_flags_one_dominant_cluster_with_crumbs(self):
        segments = np.array([[0.0, 96.0, 0], [96.0, 97.0, 1], [97.0, 98.5, 1]])

        self.assertTrue(detect_collapse(segments, self.options))

    def test_ignores_healthy_two_speaker_split(self):
        segments = np.array([[0.0, 70.0, 0], [70.0, 100.0, 1]])

        self.assertFalse(detect_collapse(segments, self.options))

    def test_ignores_single_cluster(self):
        segments = np.array([[0.0, 100.0, 0]])

        self.assertFalse(detect_collapse(segments, self.options))


class SegmentRuleTests(unittest.TestCase):
    def setUp(self):
        self.options = DiarizeOptions()

    def test_short_segment_folds_into_neighbour(self):
        segments = np.array([[0.0, 5.0, 0], [5.0, 5.2, 1], [5.2, 10.0, 0]])

        result = merge_short_segments(segments, self.options)

        self.assertEqual(int(result[1, 2]), 0)

    def test_adjacent_same_speaker_merges_across_short_pause(self):
        segments = np.array([[0.0, 5.0, 0], [5.5, 10.0, 0], [10.0, 12.0, 1]])

        result = merge_adjacent_same_speaker(segments, 1.0)

        self.assertEqual(len(result), 2)
        self.assertAlmostEqual(result[0, 1], 10.0)

    def test_adjacent_different_speaker_does_not_merge(self):
        segments = np.array([[0.0, 5.0, 0], [5.1, 10.0, 1]])

        result = merge_adjacent_same_speaker(segments, 1.0)

        self.assertEqual(len(result), 2)

    def test_default_gap_bridges_pauses_real_people_actually_make(self):
        # Regression for a measured defect: extract_segments already bridges
        # gaps up to 0.5s, so a 1.0s default merged nothing at all (on a
        # 31-minute recording every same-speaker pair sat at 1.15s+). One
        # monologue then surfaced as five cards. The default must cover the
        # observed 1.0-1.5s cluster.
        segments = np.array([[0.0, 5.0, 0], [6.6, 10.0, 0]])

        result = merge_adjacent_same_speaker(
            segments, DiarizeOptions().adjacent_merge_gap_seconds
        )

        self.assertEqual(len(result), 1)
        self.assertAlmostEqual(result[0, 1], 10.0)

    def test_default_gap_keeps_a_real_turn_pause_apart(self):
        # The other side of the same knob: 3s of silence inside one speaker's
        # row is a scene change (or a swallowed reply), not a breath.
        segments = np.array([[0.0, 5.0, 0], [8.0, 12.0, 0]])

        result = merge_adjacent_same_speaker(
            segments, DiarizeOptions().adjacent_merge_gap_seconds
        )

        self.assertEqual(len(result), 2)

    def test_default_ceiling_is_a_cost_decision_not_a_model_limit(self):
        # The model can take 115s, which is why the old default was 115. It
        # also costs 161ms per audio-second there versus ~40ms at 45s, because
        # attention is quadratic. Guard against anyone "restoring" the bigger
        # number after reading only the model spec.
        self.assertLessEqual(DiarizeOptions().max_segment_seconds, 60.0)

    def test_long_segment_is_split(self):
        segments = np.array([[0.0, 250.0, 0]])

        result = split_long_segments(segments, 115.0)

        self.assertEqual(len(result), 3)
        self.assertTrue(all(end - start <= 115.0 for start, end, _ in result))
        self.assertAlmostEqual(result[-1, 1], 250.0)


class CompactTests(unittest.TestCase):
    def test_renumbers_ids_and_reorders_centroids(self):
        # Cluster 1 was dropped, so ids are 0 and 2 and must become 0 and 1.
        segments = np.array([[0.0, 30.0, 2], [30.0, 40.0, 0]])
        centroids = np.array([[1.0, 0.0], [0.0, 1.0], [0.5, 0.5]], dtype=np.float32)

        compacted, compact_centroids = _compact(segments, centroids)

        self.assertEqual(compacted[:, 2].tolist(), [0, 1])
        # Largest first: the 30s cluster becomes id 0 and takes the [0.5,0.5] row.
        np.testing.assert_allclose(compact_centroids[0], [0.5, 0.5])
        np.testing.assert_allclose(compact_centroids[1], [1.0, 0.0])

    def test_centroids_stay_aligned_with_ids(self):
        """Regression: stale centroids used to delete every speaker but one."""
        segments = np.array([[0.0, 100.0, 0], [100.0, 140.0, 7]])
        centroids = np.zeros((9, 4), dtype=np.float32)
        centroids[0] = [1, 0, 0, 0]
        centroids[7] = [0, 1, 0, 0]

        compacted, compact_centroids = _compact(segments, centroids)

        self.assertEqual(len(compact_centroids), 2)
        for start, end, cluster in compacted:
            self.assertLess(int(cluster), len(compact_centroids))
        np.testing.assert_allclose(compact_centroids[0], [1.0, 0.0, 0.0, 0.0])
        np.testing.assert_allclose(compact_centroids[1], [0.0, 1.0, 0.0, 0.0])


class MergeSimilarClustersTests(unittest.TestCase):
    def setUp(self):
        self.options = DiarizeOptions()

    def test_merges_identical_centroids(self):
        segments = np.array([[0.0, 10.0, 0], [10.0, 20.0, 1]])
        centroids = _normalized([[1.0, 0.0], [1.0, 0.001]])

        merged, merged_centroids = merge_similar_clusters(segments, centroids, self.options)

        self.assertEqual(len(set(merged[:, 2].astype(int).tolist())), 1)
        self.assertEqual(len(merged_centroids), 1)

    def test_keeps_distinct_speakers_apart(self):
        # Measured different-person centroid cosine is 0.683; must not merge.
        segments = np.array([[0.0, 10.0, 0], [10.0, 20.0, 1]])
        angle = np.arccos(0.683)
        centroids = np.array(
            [[1.0, 0.0], [np.cos(angle), np.sin(angle)]], dtype=np.float32
        )

        merged, merged_centroids = merge_similar_clusters(segments, centroids, self.options)

        self.assertEqual(len(set(merged[:, 2].astype(int).tolist())), 2)
        self.assertEqual(len(merged_centroids), 2)


class SegmentExtractionTests(unittest.TestCase):
    def test_extracts_run_per_cluster(self):
        active = np.zeros((10, 2), dtype=bool)
        active[0:5, 0] = True
        active[6:10, 1] = True

        segments = extract_segments(active, 0.1, 0.0, None, DiarizeOptions())

        self.assertEqual(len(segments), 2)
        self.assertEqual(segments[0, 2], 0)
        self.assertEqual(segments[1, 2], 1)
        self.assertAlmostEqual(segments[0, 0], 0.0)
        self.assertAlmostEqual(segments[1, 0], 0.6)

    def test_merges_gap_shorter_than_threshold(self):
        active = np.zeros((20, 1), dtype=bool)
        active[0:5, 0] = True
        active[7:12, 0] = True

        segments = extract_segments(active, 0.1, 0.0, None, DiarizeOptions())

        self.assertEqual(len(segments), 1)

    def test_returns_empty_for_silence(self):
        segments = extract_segments(
            np.zeros((10, 1), dtype=bool), 0.01, 0.0, None, DiarizeOptions()
        )

        self.assertEqual(len(segments), 0)

    def test_frame_gap_comes_from_options(self):
        # 帧级补洞的 0.5 以前是函数默认参数，和 DiarizeOptions 里的合并阈值
        # 各管一段，调一个完全看不到另一个的效果。它必须能被单独调。
        active = np.zeros((20, 1), dtype=bool)
        active[0:5, 0] = True
        active[7:12, 0] = True  # 2 帧空隙 = 0.2s

        loose = extract_segments(active, 0.1, 0.0, None, DiarizeOptions())
        tight = extract_segments(
            active, 0.1, 0.0, None, replace(DiarizeOptions(), frame_gap_seconds=0.1)
        )

        self.assertEqual(len(loose), 1)
        self.assertEqual(len(tight), 2)


class VoteCoverageTests(unittest.TestCase):
    def test_frames_without_a_sampling_window_are_never_active(self):
        # cover=0 的帧是 stride 过大的直接后果。把阈值放到 0 来证明判据里
        # 真的有 cover>0 这一项，而不是靠 score 恰好低于 0.5 侥幸过关。
        class Segmentation:
            frame_count = 6
            frames_per_window = 3
            window_count = 2
            receptive_shift_samples = 1600
            window_offsets = np.array([0, 9600])
            window_labels = np.ones((2, 3, 1), dtype=np.float32)

        diarizer = Diarizer(_FakeSegmenter(), _FakeEmbedder())
        _, active = diarizer._vote(
            Segmentation(),
            np.array([[0, 0]]),
            np.array([0]),
            1,
            replace(DiarizeOptions(), vote_threshold=0.0),
        )

        self.assertEqual(active[:, 0].tolist(), [True, True, True, False, False, False])


class OptionOverrideTests(unittest.TestCase):
    def test_no_env_keeps_defaults(self):
        options, locked = load_options_from_env(environ={})

        self.assertEqual(options, DiarizeOptions())
        self.assertEqual(locked, frozenset())

    def test_known_key_is_applied_and_reported(self):
        options, locked = load_options_from_env(
            environ={DIARIZE_OVERRIDES_ENV: '{"adjacent_merge_gap_seconds": 1.5}'}
        )

        self.assertEqual(options.adjacent_merge_gap_seconds, 1.5)
        self.assertEqual(locked, frozenset({"adjacent_merge_gap_seconds"}))
        self.assertEqual(
            options.max_segment_seconds, DiarizeOptions().max_segment_seconds
        )

    def test_unknown_key_is_an_error_not_silently_ignored(self):
        # 静默忽略最难受：以为调了，其实没生效。
        with self.assertRaises(ValueError) as caught:
            load_options_from_env(environ={DIARIZE_OVERRIDES_ENV: '{"merge_gap": 3.0}'})

        self.assertIn("merge_gap", str(caught.exception))

    def test_out_of_range_is_rejected(self):
        # 0.5 是 openwhispr 的值，实测会把两个人并成一个；必须拦在入口。
        with self.assertRaises(ValueError):
            load_options_from_env(
                environ={DIARIZE_OVERRIDES_ENV: '{"merge_centroid_cos": 0.5}'}
            )

    def test_int_key_rejects_fraction(self):
        with self.assertRaises(ValueError):
            load_options_from_env(
                environ={DIARIZE_OVERRIDES_ENV: '{"embed_window_stride": 2.5}'}
            )

    def test_malformed_json_is_rejected(self):
        with self.assertRaises(ValueError):
            load_options_from_env(environ={DIARIZE_OVERRIDES_ENV: "{not json}"})

    def test_combination_that_makes_a_threshold_dead_is_rejected(self):
        # 每个字段都合法，组合起来却让段级合并永不触发——这正是 1.0 当年
        # 变成空阈值的方式，必须在入口拦住。
        with self.assertRaises(ValueError):
            load_options_from_env(
                environ={DIARIZE_OVERRIDES_ENV: '{"frame_gap_seconds": 3.0}'}
            )

    def test_non_object_payload_is_rejected(self):
        with self.assertRaises(ValueError):
            load_options_from_env(environ={DIARIZE_OVERRIDES_ENV: "[1, 2]"})

    def test_every_option_has_a_bounds_entry(self):
        # 结构性防漏：新增字段忘了进覆盖表，它就既不能调也没有范围保护，
        # 而且没人会注意到。这条测试替代码检查。
        missing = sorted(
            field.name
            for field in fields(DiarizeOptions)
            if field.name not in _OVERRIDE_BOUNDS
        )

        self.assertEqual(missing, [])


class AdaptiveOptionTests(unittest.TestCase):
    def _adapt(self, **overrides):
        params = {
            "backend": "cuda",
            "gpu_memory_total_gb": 4.0,
            "duration_seconds": 1860.0,
        }
        params.update(overrides)
        return adapt_options(DiarizeOptions(), **params)

    def test_small_gpu_keeps_the_45s_ceiling(self):
        self.assertEqual(self._adapt().max_segment_seconds, 45.0)

    def test_big_gpu_raises_the_ceiling(self):
        self.assertEqual(self._adapt(gpu_memory_total_gb=12.0).max_segment_seconds, 60.0)

    def test_unknown_vram_falls_back_to_default(self):
        self.assertEqual(self._adapt(gpu_memory_total_gb=None).max_segment_seconds, 45.0)

    def test_cpu_backend_never_raises_the_ceiling(self):
        self.assertEqual(
            self._adapt(backend="cpu", gpu_memory_total_gb=16.0).max_segment_seconds, 45.0
        )

    def test_very_long_audio_stays_at_45_even_on_a_big_gpu(self):
        # 2 小时：60s 段按成本模型要 ~412s，超出「60s + 3% 时长」的 276s 预算，
        # 45s 段只要 ~257s，所以落回 45。预算判据在这里才真正起作用。
        self.assertEqual(
            self._adapt(
                gpu_memory_total_gb=12.0, duration_seconds=7200.0
            ).max_segment_seconds,
            45.0,
        )

    def test_stride_stays_safe_even_for_long_audio(self):
        # 回归：按「长音频放粗」推到 6/8 时，10s/2s 模型的采样窗相隔 12/16s，
        # 每 12s 留 2s 无覆盖洞，58 分钟录音上吞掉了「全党高度团结统一」。
        for duration in (1000.0, 3000.0, 6000.0, 12000.0):
            options = self._adapt(duration_seconds=duration)
            self.assertEqual(options.embed_window_stride, 4)
            self.assertLess(
                options.embed_window_stride * _FakeSegmenter.window_shift_samples,
                _FakeSegmenter.window_samples,
            )

    def test_cpu_backend_does_not_coarsen_stride(self):
        # CPU 慢的是算力，不是覆盖倍数；放粗步长同样会留洞。
        self.assertEqual(self._adapt(backend="cpu").embed_window_stride, 4)

    def test_coverage_limit_comes_from_the_model(self):
        self.assertEqual(max_safe_embed_stride(160_000, 32_000), 4)
        self.assertEqual(max_safe_embed_stride(160_000, 16_000), 9)
        self.assertEqual(max_safe_embed_stride(0, 0), 0)

    def test_runtime_clamps_an_explicit_stride(self):
        # 环境变量锁定 stride=8 时 adapt_options 尊重人，但最后一道防线
        # 仍要按真实模型把它压回 4，否则旧参数一样会吞字。
        embedder = _FakeEmbedder()
        diarizer = Diarizer(_FakeSegmenter(), embedder)
        segmentation = _FakeSegmentation(windows=16)

        diarizer._embed_windows(
            _FakeAudio(),
            segmentation,
            None,
            replace(DiarizeOptions(), embed_window_stride=8),
        )

        # stride 4 + 补最后一窗 = 0,4,8,12,15 共 5 次；stride 8 只会 3 次。
        self.assertEqual(embedder.calls, 5)
        self.assertEqual(diarizer._effective_embed_stride(DiarizeOptions()), 4)

    def test_locked_keys_survive_adaptation(self):
        # 人是来调参的，自动化不该把人填的值改回去。
        options, locked = load_options_from_env(
            environ={
                DIARIZE_OVERRIDES_ENV: (
                    '{"max_segment_seconds": 30.0, "embed_window_stride": 2}'
                )
            }
        )

        adapted = adapt_options(
            options,
            backend="cuda",
            gpu_memory_total_gb=12.0,
            duration_seconds=6000.0,
            locked=locked,
        )

        self.assertEqual(adapted.max_segment_seconds, 30.0)
        self.assertEqual(adapted.embed_window_stride, 2)

    def test_per_call_options_reach_the_embedding_stride(self):
        # 回归：helper 读的是 `self.options`（构造时的基线）而不是 run 传进来的
        # 本次参数时，自适应推导出的步长完全不生效，而且没有任何报错。
        embedder = _FakeEmbedder()
        diarizer = Diarizer(_FakeSegmenter(), embedder)
        segmentation = _FakeSegmentation()

        diarizer._embed_windows(
            _FakeAudio(),
            segmentation,
            None,
            replace(DiarizeOptions(), embed_window_stride=1),
        )
        dense = embedder.calls
        embedder.calls = 0
        diarizer._embed_windows(
            _FakeAudio(),
            segmentation,
            None,
            replace(DiarizeOptions(), embed_window_stride=4),
        )
        sparse = embedder.calls

        self.assertEqual(dense, 8)
        # stride 4 + 补最后一窗 = 3 次（0、4、7）。
        self.assertEqual(sparse, 3)


class PowersetTests(unittest.TestCase):
    def test_powerset_layout_matches_pyannote(self):
        matrix = _powerset_matrix(7, 3)

        np.testing.assert_array_equal(matrix[0], [0, 0, 0])
        np.testing.assert_array_equal(matrix[1], [1, 0, 0])
        np.testing.assert_array_equal(matrix[2], [0, 1, 0])
        np.testing.assert_array_equal(matrix[3], [0, 0, 1])
        np.testing.assert_array_equal(matrix[4], [1, 1, 0])
        np.testing.assert_array_equal(matrix[5], [1, 0, 1])
        np.testing.assert_array_equal(matrix[6], [0, 1, 1])

    def test_softmax_rows_sum_to_one(self):
        logits = np.array([[1.0, 2.0, 3.0, 0.0, -1.0, 2.5, 0.5]], dtype=np.float32)

        probs = _softmax(logits)

        self.assertAlmostEqual(float(probs.sum()), 1.0, places=5)


if __name__ == "__main__":
    unittest.main()
