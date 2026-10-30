#!/usr/bin/env python3
"""Per-seed comparison with aborted/oracle-invalid leases excluded, never zero-filled."""
import argparse
import csv
import json
from pathlib import Path

from report import median_present


def exclusion(row):
    if row.get('status') in ('fixture_error', 'aborted', 'unavailable'):
        return 'fixture_unavailable'
    if row.get('source_remote_after') is not True:
        return 'lease_aborted'
    if (row.get('status') != 'ok' or row.get('final_position_delivered') is not True
            or any(row.get(key, 0) for key in ('wrong_click_count', 'wrong_order_count', 'lease_leak_count'))):
        return 'position_or_order_oracle_failed'
    if not row.get('pub_to_apply', {}).get('count'):
        return 'no_samples'
    return None


def summarize(inputs):
    per_seed = []
    for side, rows in inputs.items():
        for row in rows:
            if row['case'] != 'matrix' or row['delay_ms'] != 0 or row['loss_pct'] not in (1, 5):
                continue
            why = exclusion(row)
            age = row.get('pub_to_apply', {})
            per_seed.append({'side': side, 'loss_pct': row['loss_pct'], 'seed': row['seed'],
                             'excluded': why, 'final_delivered': row.get('final_position_delivered'),
                             'sample_count': age.get('count', 0),
                             'observed_p95_ms': age.get('p95_ms'), 'observed_p99_ms': age.get('p99_ms'),
                             'completed_p95_ms': None if why else age.get('p95_ms'),
                             'completed_p99_ms': None if why else age.get('p99_ms'),
                             'stalls': row.get('stalls_over_50ms_count'),
                             'stall_excess_ms': row.get('stall_excess_ms'),
                             'wrong_clicks': row.get('wrong_click_count'),
                             'wrong_order': row.get('wrong_order_count'), 'cross_lease': row.get('lease_leak_count')})
    aggregate = []
    matched = []
    for loss in (1, 5):
        good = {}
        for side in inputs:
            selected = [row for row in per_seed if row['side'] == side and row['loss_pct'] == loss]
            valid = [row for row in selected if row['excluded'] is None]
            good[side] = {row['seed']: row for row in valid}
            aggregate.append({'side': side, 'loss_pct': loss, 'runs': len(selected),
                              'completed_valid_seeds': sorted(good[side]),
                              'excluded_count': len(selected)-len(valid),
                              'completed_seed_median_p95_ms': median_present(row['completed_p95_ms'] for row in valid),
                              'completed_seed_median_p99_ms': median_present(row['completed_p99_ms'] for row in valid)})
        seeds = set.intersection(*(set(value) for value in good.values())) if good else set()
        matched.append({'loss_pct': loss, 'completed_on_both_sides': sorted(seeds)})
    return {'measurement': 'same-kernel capture publication to dry injection completion',
            'quantiles': 'median per-seed quantiles, NOT pooled; excluded leases retain observed diagnostics but no completed latency',
            'per_seed': per_seed, 'completed_only': aggregate, 'matched_seeds': matched}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('before')
    parser.add_argument('after')
    parser.add_argument('--json', required=True)
    parser.add_argument('--csv', required=True)
    args = parser.parse_args()
    inputs = {side: [json.loads(line) for line in Path(path).read_text().splitlines() if line.strip()]
              for side, path in (('before', args.before), ('after', args.after))}
    result = summarize(inputs)
    Path(args.json).write_text(json.dumps(result, indent=2)+'\n', encoding='utf-8')
    with Path(args.csv).open('w', newline='', encoding='utf-8') as stream:
        writer = csv.DictWriter(stream, fieldnames=list(result['per_seed'][0]))
        writer.writeheader()
        writer.writerows(result['per_seed'])
    print(json.dumps(result, indent=2))


if __name__ == '__main__':
    main()
