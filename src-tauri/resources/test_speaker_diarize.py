import os
import sys
import unittest

import numpy as np

sys.path.insert(0, os.path.dirname(__file__))

from speaker_diarize import (
    DiarizeOptions,
    _compact,
    average_linkage,
    cluster_embeddings,
    detect_collapse,
    drop_phantom_clusters,
    extract_segments,
    merge_adjacent_same_speaker,
    merge_short_segments,
    merge_similar_clusters,
    split_long_segments,
)
from speaker_segmentation import _powerset_matrix, _softmax


def _normalized(rows):
    matrix = np.asarray(rows, dtype=np.float32)
    return matrix / np.linalg.norm(matrix, axis=1, keepdims=True)


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
