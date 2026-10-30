"""Analysis-only regressions; no processes, network impairment, or native input."""
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import report
from test_observe import blob, record, scalar


class ReportTests(unittest.TestCase):
    def release_us(self, replies, end_ns=1_000_000_000):
        lease = blob(1, b'L' * 16)
        prefix = record(1, 0, 15, lease + scalar(2, 1000))
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'receiver.obs'
            path.write_bytes(prefix + replies)
            return report.receiver_release_us(path, 100_000_000, end_ns)

    def test_waits_for_release_completion_not_command_read(self):
        lease = blob(1, b'L' * 16)
        replies = record(200_000_000, 0, 16, lease) + record(500_000_000, 1, 46, lease)
        self.assertEqual(self.release_us(replies), 400_000)

    def test_ignores_other_lease_reply(self):
        replies = (record(150_000_000, 1, 46, blob(1, b'N' * 16))
                   + record(300_000_000, 1, 45, blob(1, b'L' * 16)))
        self.assertEqual(self.release_us(replies), 200_000)

    def test_missing_release_is_unavailable_not_zero(self):
        self.assertIsNone(self.release_us(record(200_000_000, 0, 16, blob(1, b'L' * 16))))

    def test_rounds_upper_bound_up_and_respects_original_window(self):
        reply = record(100_001_250, 1, 46, blob(1, b'L' * 16))
        self.assertEqual(self.release_us(reply), 2)
        self.assertIsNone(self.release_us(reply, end_ns=100_001_000))

    def test_missing_quantiles_are_not_zero_latency(self):
        self.assertIsNone(report.median_present([None, None]))
        self.assertEqual(report.median_present([None, 10, 30]), 20)

    def test_quantiles_are_seed_medians_not_sample_weighted(self):
        rows = [{'pub_to_apply': {'count': 100, 'p95_ms': 10, 'p99_ms': 20}},
                {'pub_to_apply': {'count': 1, 'p95_ms': 30, 'p99_ms': 40}}]
        aggregate = report.aggregate(rows)
        self.assertEqual(aggregate['latency_sample_count'], 101)
        self.assertEqual(aggregate['pub_to_apply_seed_median_p95_ms'], 20)
        self.assertEqual(aggregate['pub_to_apply_seed_median_p99_ms'], 30)

    def test_overall_counts_do_not_mix_latency_across_loss_profiles(self):
        rows = [dict(case='matrix', loss_pct=loss, delay_ms=0, jitter_ms=0,
                     pub_to_apply={'count': 1, 'p95_ms': age, 'p99_ms': age})
                for loss, age in ((1, 2), (5, 500))]
        with patch.object(report, 'rows', side_effect=[rows, rows]):
            result = report.report('before', 'after')
        for side in ('before', 'after'):
            self.assertNotIn('pub_to_apply_seed_median_p95_ms', result['all_' + side])
            self.assertNotIn('pub_to_apply_seed_median_p99_ms', result['all_' + side])
            self.assertEqual(result['all_' + side]['runs'], 2)
            self.assertEqual([row[side]['pub_to_apply_seed_median_p95_ms']
                              for row in result['matrix']], [2, 500])


if __name__ == '__main__':
    unittest.main()
