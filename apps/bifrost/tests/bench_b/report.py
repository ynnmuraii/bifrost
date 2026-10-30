#!/usr/bin/env python3
"""Aggregate bench B without pretending that medians of seed quantiles are pooled."""
import argparse
import csv
import json
from pathlib import Path
import statistics

from observe import one, records


def receiver_release_us(path, fault_ns, drain_end_ns):
    """Successful release reply, not read-ahead receipt of ReleaseSession."""
    held = None
    for ns, direction, tag, _seq, _pid, message in records(path):
        lease = one(message, 1, b"")
        if ns < fault_ns:
            if direction == 0 and tag == 15:
                held = lease
            elif direction == 1 and tag in (45, 46) and lease == held:
                held = None
        elif ns <= drain_end_ns and held and direction == 1 and tag in (45, 46) and lease == held:
            # Round upward: this is an observed upper bound on release completion.
            return (ns - fault_ns + 999) // 1000
    return None

def rows(path):
    result = [json.loads(line) for line in Path(path).read_text().splitlines() if line.strip()]
    for row in result:
        if row["case"] == "matrix" or row.get("status") in ("fixture_error", "aborted", "unavailable"):
            continue
        artifacts = Path(path).parent / f'{row["side"]}-{row["case"]}'
        window = json.loads((artifacts / "window.json").read_text())
        receiver_us = receiver_release_us(artifacts / "receiver.obs", window["fault_start_ns"],
                                          window["delivery_drain_end_ns"])
        source_us = row.get("control_return_us")
        row["receiver_control_return_us"] = receiver_us
        row["both_ends_control_return_within_ttl_plus_100"] = (
            max(source_us, receiver_us) <= 1_100_000
            if source_us is not None and receiver_us is not None else None
        )
    return result


def median_present(values):
    present = [value for value in values if value is not None]
    return statistics.median(present) if present else None


def aggregate(items, *, include_latency=True):
    result = {
        "runs": len(items),
        "fixture_errors": sum(row.get("status") in ("fixture_error", "aborted", "unavailable") for row in items),
        "remote_at_end": sum(row.get("source_remote_after") is True for row in items),
        "final_position_delivered": sum(row.get("final_position_delivered") is True for row in items),
        "final_update_observed_at_agent": sum(row.get("final_update_observed_at_agent") is True for row in items),
        "source_event_count": sum(row.get("source_event_count", 0) for row in items),
        "latency_sample_count": sum(row.get("pub_to_apply", {}).get("count", 0) for row in items),
        "stalls_over_50ms_total": sum(row.get("stalls_over_50ms_count") or 0 for row in items),
        "stall_excess_ms_total": sum(row.get("stall_excess_ms") or 0 for row in items),
        "wrong_click_count": sum(row.get("wrong_click_count", 0) for row in items),
        "wrong_order_count": sum(row.get("wrong_order_count", 0) for row in items),
        "lease_leak_count": sum(row.get("lease_leak_count", 0) for row in items),
        "missing_discrete_count": sum(row.get("missing_discrete_count", 0) for row in items),
        "ambiguous_pointer_match_count": sum(row.get("ambiguous_pointer_match_count", 0) for row in items),
        "control_return_us": [row.get("control_return_us") for row in items],
        "control_return_acceptance": [row.get("control_return_within_ttl_plus_100") for row in items],
        "receiver_control_return_us": [row.get("receiver_control_return_us") for row in items],
        "both_ends_control_return_acceptance": [row.get("both_ends_control_return_within_ttl_plus_100") for row in items],
        "blackout_survived": [row.get("blackout_survived") for row in items],
        "recovery_final_position_delivered": [row.get("recovery_final_position_delivered") for row in items],
    }
    if include_latency:
        result.update({
            "pub_to_apply_seed_median_p50_ms": median_present(row.get("pub_to_apply", {}).get("p50_ms") for row in items),
            "pub_to_apply_seed_median_p95_ms": median_present(row.get("pub_to_apply", {}).get("p95_ms") for row in items),
            "pub_to_apply_seed_median_p99_ms": median_present(row.get("pub_to_apply", {}).get("p99_ms") for row in items),
        })
    return result


def report(before_path, after_path):
    before, after = rows(before_path), rows(after_path)
    keys = sorted({(row["loss_pct"], row["delay_ms"], row["jitter_ms"])
                   for row in before + after if row["case"] == "matrix"})
    table = []
    for loss, delay, jitter in keys:
        selected = lambda data: [row for row in data if row["case"] == "matrix"
                                 and (row["loss_pct"], row["delay_ms"], row["jitter_ms"]) == (loss, delay, jitter)]
        table.append({"loss_pct": loss, "delay_ms": delay, "jitter_ms": jitter,
                      "before": aggregate(selected(before)), "after": aggregate(selected(after))})
    safety = []
    for case in ("stalled_reader", "blackout_500ms"):
        safety.append({"case": case,
                       "before": aggregate([row for row in before if row["case"] == case]),
                       "after": aggregate([row for row in after if row["case"] == case])})
    return {"measurement": "same-kernel source CapturedBatch publish to dry receiver InjectResult",
            "quantiles": "median of per-seed quantiles; NOT pooled quantiles",
            "ambiguous_positions": "excluded from timing/order identity inference; discrete order and click position remain checked",
            "matrix": table, "safety": safety,
            "all_before": aggregate(before, include_latency=False),
            "all_after": aggregate(after, include_latency=False)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("before")
    parser.add_argument("after")
    parser.add_argument("--json", required=True)
    parser.add_argument("--csv", required=True)
    args = parser.parse_args()
    result = report(args.before, args.after)
    Path(args.json).write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    with Path(args.csv).open("w", newline="", encoding="utf-8") as stream:
        writer = csv.writer(stream)
        writer.writerow(["loss_pct", "delay_ms", "jitter_ms", "side", "runs", "remote_at_end", "final_delivered",
                         "seed_median_pub_to_apply_p95_ms", "seed_median_pub_to_apply_p99_ms", "stall_count", "stall_excess_ms",
                         "wrong_clicks", "wrong_order", "cross_lease_leaks", "missing_discrete", "fixture_errors"])
        for row in result["matrix"]:
            for side in ("before", "after"):
                value = row[side]
                writer.writerow([row["loss_pct"], row["delay_ms"], row["jitter_ms"], side,
                                 value["runs"], value["remote_at_end"], value["final_position_delivered"],
                                 value["pub_to_apply_seed_median_p95_ms"], value["pub_to_apply_seed_median_p99_ms"],
                                 value["stalls_over_50ms_total"], value["stall_excess_ms_total"], value["wrong_click_count"],
                                 value["wrong_order_count"], value["lease_leak_count"], value["missing_discrete_count"], value["fixture_errors"]])
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
