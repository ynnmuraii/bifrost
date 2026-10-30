"""Prevent aborted or frozen leases from appearing as successful low-latency seeds."""
import unittest
from seed_report import exclusion, summarize


def row(seed, age, **changes):
    value = {'case': 'matrix', 'loss_pct': 5, 'delay_ms': 0, 'seed': seed,
             'status': 'ok', 'source_remote_after': True, 'final_position_delivered': True,
             'pub_to_apply': {'count': 100, 'p95_ms': age, 'p99_ms': age+10}}
    value.update(changes)
    return value


class SeedReportTests(unittest.TestCase):
    def test_aborted_quantiles_remain_diagnostic_not_completed(self):
        value = row(23, 500, source_remote_after=False, status='fail')
        result = summarize({'before': [value], 'after': []})
        self.assertEqual(result['per_seed'][0]['excluded'], 'lease_aborted')
        self.assertIsNone(result['per_seed'][0]['completed_p99_ms'])
        self.assertEqual(result['per_seed'][0]['observed_p99_ms'], 510)

    def test_frozen_position_is_not_a_completed_latency_sample(self):
        self.assertEqual(exclusion(row(23, 1, final_position_delivered=False)),
                         'position_or_order_oracle_failed')

    def test_completed_medians_exclude_aborts_and_match_seed_identity(self):
        result = summarize({'before': [row(17, 20), row(23, 40)],
                            'after': [row(17, 30), row(23, 500, source_remote_after=False)]})
        self.assertEqual(result['completed_only'][2]['completed_seed_median_p95_ms'], 30)
        self.assertEqual(result['completed_only'][3]['completed_seed_median_p95_ms'], 30)
        self.assertEqual(result['matched_seeds'][1]['completed_on_both_sides'], [17])

    def test_missing_measurements_are_not_zero_latency(self):
        self.assertEqual(exclusion(row(17, 1, pub_to_apply={'count': 0})), 'no_samples')
        result = summarize({'before': [], 'after': []})
        self.assertTrue(all(item['completed_seed_median_p95_ms'] is None
                            for item in result['completed_only']))


if __name__ == '__main__':
    unittest.main()
