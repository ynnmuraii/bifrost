"""Private dry-fixture IPC oracle; wire numbers shared by schema 0.3 and 1.0.

The files contain synthetic fixture events, not product telemetry. Public results
contain counts/durations only. pub_to_apply is same-kernel CLOCK_MONOTONIC from
CapturedBatch successful send to InjectResult successful send, not hardware time.
"""
from __future__ import annotations

import bisect
import collections
import math
import random
import struct
from pathlib import Path

HEADER = struct.Struct('<4sBBHQQII')
ALLOWED = {0, 1, 11, 12, 13, 14, 15, 16, 40, 41, 42, 44, 45, 46, 51}
MAX_EVENTS = 500_000
MAX_RECORD_BYTES = 128 * 1024 * 1024


def fields(data):
    """Decode protobuf scalars/bytes, rejecting truncation and unbounded varints."""
    at = 0

    def varint():
        nonlocal at
        value = 0
        for shift in range(0, 70, 7):
            if at >= len(data):
                raise ValueError('truncated varint')
            byte = data[at]
            at += 1
            if shift == 63 and byte > 1:
                raise ValueError('overflowed varint')
            value |= (byte & 127) << shift
            if byte < 128:
                return value
        raise ValueError('unbounded varint')

    result = collections.defaultdict(list)
    while at < len(data):
        key = varint()
        number, wire = key >> 3, key & 7
        if not number:
            raise ValueError('zero protobuf field')
        if wire == 0:
            value = varint()
        elif wire in (1, 2, 5):
            size = varint() if wire == 2 else (8 if wire == 1 else 4)
            if size > len(data) - at:
                raise ValueError('truncated protobuf field')
            value = data[at:at + size]
            at += size
        else:
            raise ValueError('unsupported protobuf wire type')
        result[number].append(value)
    return result


def one(message, number, default=0):
    return message.get(number, [default])[-1]


def pointer(message):
    p = fields(message)
    x = struct.unpack('<d', one(p, 2, b'\0' * 8))[0]
    y = struct.unpack('<d', one(p, 3, b'\0' * 8))[0]
    if not math.isfinite(x) or not math.isfinite(y):
        raise ValueError('nonfinite pointer')
    return x, y


def events(message, number):
    for raw in message.get(number, []):
        event = fields(raw)
        kinds = [k for k in (10, 11, 12, 13) if k in event]
        if len(kinds) != 1:
            raise ValueError('missing or ambiguous event')
        kind = kinds[0]
        value = one(event, kind)
        if kind == 10:
            yield ('pointer', pointer(value))
        else:
            body = fields(value)
            count = 3 if kind == 11 else 2
            # Numeric synthetic key/button/wheel identity is retained only in memory.
            yield ({11: 'key', 12: 'button', 13: 'wheel'}[kind],
                   tuple(one(body, k) for k in range(1, count + 1)))


def records(path):
    if Path(path).stat().st_size > MAX_RECORD_BYTES:
        raise ValueError('observer byte bound exceeded')
    with Path(path).open('rb') as stream:
        for _ in range(MAX_EVENTS):
            raw = stream.read(HEADER.size)
            if not raw:
                return
            if len(raw) != HEADER.size:
                raise ValueError('truncated observer header')
            magic, direction, tag, reserved, ns, seq, pid, size = HEADER.unpack(raw)
            if magic != b'BBO1' or direction > 1 or tag not in ALLOWED or reserved or size > 65536:
                raise ValueError('invalid observer record')
            body = stream.read(size)
            if len(body) != size:
                raise ValueError('truncated observer payload')
            if tag == 0:
                raise ValueError('observer framing failure')
            yield ns, direction, tag, seq, pid, fields(body)
        raise ValueError('observer record bound exceeded')


def close_point(a, b, tolerance):
    return a is not None and b is not None and max(abs(a[0] - b[0]), abs(a[1] - b[1])) <= tolerance


def point_key(position):
    return tuple(round(v, 5) for v in position)


class Samples:
    """Deterministic bounded reservoir; total count remains exact."""
    def __init__(self, limit):
        self.limit, self.count, self.values = limit, 0, []
        self.rng = random.Random(17)

    def add(self, value):
        self.count += 1
        if len(self.values) < self.limit:
            self.values.append(value)
        else:
            index = self.rng.randrange(self.count)
            if index < self.limit:
                self.values[index] = value

    def report(self):
        values = sorted(self.values)
        return {'count': self.count, 'sample_count': len(values),
                'sampling': 'exact' if self.count == len(values) else 'reservoir_seed17',
                **{f'p{p}_ms': (values[max(0, math.ceil(len(values) * p / 100) - 1)] / 1e6
                                if values else None) for p in (50, 95, 99)}}


class Observer:
    def __init__(self, path_a, path_b):
        self.path_a, self.path_b = path_a, path_b

    def analyze(self, measured_start_ns, measured_end_ns, **options):
        return analyze(self.path_a, self.path_b, measured_start_ns, measured_end_ns, **options)


def analyze(source_path, receiver_path, measured_start_ns, measured_end_ns, *,
            source_seed=(960.0, 540.0), target_start=(960.0, 540.0),
            pointer_tolerance=0.001, max_samples=20000, stall_threshold_ms=50.0,
            final_source_before_ns=None):
    """Analyze after stopping/draining agents; never return a fabricated clean pass.

    Latency samples require source publication and successful completion inside the
    measured interval. Final delivery uses the latest observed source capture,
    including drain. Receiver startup/entry warps (batch 0) are not source events.
    """
    result = {'status': 'unavailable', 'reason': None,
              'measurement': 'same_kernel_captured_publish_to_inject_result',
              'measured_duration_ms': (measured_end_ns - measured_start_ns) / 1e6,
              'wrong_click_count': 0, 'wrong_order_count': 0, 'lease_leak_count': 0,
              'nonmonotonic_batch_count': 0, 'inject_error_count': 0,
              'source_event_count': 0, 'source_pointer_count': 0,
              'receiver_applied_event_count': 0, 'unmatched_receiver_event_count': 0,
              'unobserved_source_event_count': 0, 'coalesced_or_dropped_pointer_count': 0,
              'missing_discrete_count': 0, 'unacknowledged_batch_count': 0,
              'ambiguous_pointer_match_count': 0,
              'stalls_over_50ms_count': None, 'stall_excess_ms': None,
              'max_active_gap_ms': None, 'final_position_delivered': None}
    result['pub_to_apply'] = Samples(max(1, max_samples)).report()
    result['pub_to_apply_discrete'] = Samples(max(1, max_samples)).report()
    if measured_end_ns <= measured_start_ns or max_samples <= 0 or stall_threshold_ms <= 0:
        result.update(status='aborted', reason='invalid measured interval or limits')
        return result
    try:
        source = list(records(source_path))
        receiver = list(records(receiver_path))
        if not source or not receiver:
            result['reason'] = 'missing observer stream'
            return result
        # Source events are indexed by transformed positions and discrete order per lease.
        positions = collections.defaultdict(list)
        discrete = collections.defaultdict(list)
        source_items = []
        last_source = tuple(source_seed)
        origin = tuple(source_seed)
        expected = tuple(target_start)
        lease = None
        remote_ready = False
        begun = None
        source_errors = 0
        for ns, direction, tag, seq, pid, body in source:
            if tag == 51:
                source_errors += 1
            if direction == 0 and tag == 11:
                begun = one(body, 2, b'') if one(body, 1) == 3 else None
                lease, remote_ready = None, False
            elif direction == 1 and tag == 41 and one(body, 2) == 3:
                lease = one(body, 1, b'')
                if not lease or lease != begun:
                    raise ValueError('unmatched Remote CaptureReady')
                origin, expected, remote_ready = last_source, tuple(target_start), True
            elif direction == 0 and tag == 12:
                if one(body, 1, b'') == lease:
                    remote_ready, lease = False, None
            elif direction == 1 and tag == 42:
                for kind, value in events(body, 1):
                    if kind == 'pointer':
                        last_source = value
                    if not remote_ready:
                        continue
                    if kind == 'pointer':
                        expected = (target_start[0] + value[0] - origin[0],
                                    target_start[1] + value[1] - origin[1])
                    item = {'ns': ns, 'kind': kind, 'value': value, 'position': expected,
                            'lease': lease, 'seen': False}
                    item['ordinal'] = len(source_items)
                    source_items.append(item)
                    if len(source_items) > MAX_EVENTS:
                        raise ValueError('source event bound exceeded')
                    if kind == 'pointer':
                        positions[(lease, point_key(expected))].append(item)
                    else:
                        discrete[lease].append(item)
        if not source_items:
            result['reason'] = 'no Remote source captures'
            return result
        measured = [x for x in source_items if measured_start_ns <= x['ns'] <= measured_end_ns]
        if not measured:
            result['reason'] = 'no source captures in measured interval'
            return result
        result['source_event_count'] = len(measured)
        result['source_pointer_count'] = sum(x['kind'] == 'pointer' for x in measured)
        sample = Samples(max_samples)
        discrete_sample = Samples(max_samples)
        pending = {}
        preparing = collections.deque()
        prepared = set()
        active = {}
        high_seq = collections.defaultdict(lambda: -1)
        discrete_index = collections.defaultdict(int)
        last_position = {}
        last_event_order = collections.defaultdict(lambda: -1)
        applied_times = []
        applied_positions = []
        final = next(
            item for item in reversed(source_items)
            if final_source_before_ns is None or item['ns'] <= final_source_before_ns
        )
        # Position lookup is O(log n) even when the dry jitter repeats forever.
        pointer_times = {key: [item['ns'] for item in items] for key, items in positions.items()}
        for ns, direction, tag, seq, pid, body in receiver:
            lid = one(body, 1, b'')
            if tag == 51:
                result['inject_error_count'] += 1
            if tag == 1:
                prepared.clear(); active.clear(); preparing.clear()
            elif direction == 0 and tag == 13:
                preparing.append(lid)
            elif direction == 1 and tag == 40 and preparing:
                # Socket reads can contain several commands ahead of their execution.
                # Ready/Released replies, not read-ahead receipt, establish transitions.
                prepared.clear()
                active.clear()
                prepared.add(preparing.popleft())
            elif direction == 0 and tag == 15:
                if lid not in prepared:
                    result['lease_leak_count'] += 1
                active[lid] = True
            elif direction == 1 and tag in (45, 46):
                active.pop(lid, None)
                if tag in (45, 46):
                    prepared.discard(lid)
            elif direction == 0 and tag == 14:
                batch = one(body, 2)
                evs = list(events(body, 3))
                valid = lid in prepared and lid in active
                # PrepareInject permits entry warp batch zero before ArmDeadline.
                if batch == 0:
                    valid = lid in prepared
                if not valid:
                    result['lease_leak_count'] += len(evs) or 1
                if batch <= high_seq[lid]:
                    result['nonmonotonic_batch_count'] += 1
                high_seq[lid] = max(high_seq[lid], batch)
                key = (lid, batch)
                if key in pending:
                    raise ValueError('duplicate pending Inject')
                pending[key] = (ns, evs, valid)
            elif direction == 1 and tag == 44:
                batch = one(body, 2)
                request = pending.pop((lid, batch), None)
                if request is None:
                    raise ValueError('InjectResult without observed Inject')
                sent, evs, valid = request
                count = one(body, 3)
                if count > len(evs) or ns < sent:
                    raise ValueError('invalid InjectResult count or clock')
                if one(body, 4, b''):
                    result['inject_error_count'] += 1
                # Failed/partial batches are reported, never latency successes.
                if count != len(evs):
                    result['inject_error_count'] += 1
                if count != len(evs) or one(body, 4, b''):
                    continue
                if batch == 0:
                    for kind, value in evs[:count]:
                        if kind == 'pointer':
                            last_position[lid] = value
                    continue
                if not valid:
                    continue
                if lid not in active:
                    result['lease_leak_count'] += count or 1
                    continue
                for kind, value in evs[:count]:
                    if measured_start_ns <= ns <= measured_end_ns:
                        result['receiver_applied_event_count'] += 1
                    match = None
                    ambiguous = False
                    if kind == 'pointer':
                        key = (lid, point_key(value))
                        candidates = positions.get(key, [])
                        index = bisect.bisect_right(pointer_times.get(key, []), sent) - 1
                        if index >= 0:
                            match = candidates[index]
                            ambiguous = index > 0
                            if ambiguous and measured_start_ns <= ns <= measured_end_ns:
                                result['ambiguous_pointer_match_count'] += 1
                        last_position[lid] = value
                        applied_positions.append((ns, lid, value))
                    else:
                        index = discrete_index[lid]
                        candidates = discrete[lid]
                        if index < len(candidates):
                            wanted = candidates[index]
                            if wanted['kind'] == kind and wanted['value'] == value and wanted['ns'] <= sent:
                                match = wanted
                                discrete_index[lid] += 1
                                if kind == 'button' and not close_point(last_position.get(lid), wanted['position'], pointer_tolerance):
                                    result['wrong_click_count'] += 1
                            else:
                                result['wrong_order_count'] += 1
                        else:
                            result['wrong_order_count'] += 1
                    if match is None:
                        result['unmatched_receiver_event_count'] += 1
                        continue
                    # Repeated identical positions have no wire event identity. A later
                    # publication cannot prove that an earlier delayed frame was reordered.
                    if not ambiguous:
                        if match['ordinal'] < last_event_order[lid]:
                            result['wrong_order_count'] += 1
                        last_event_order[lid] = max(last_event_order[lid], match['ordinal'])
                    if match['seen'] and kind != 'pointer':
                        result['wrong_order_count'] += 1
                    match['seen'] = True
                    if measured_start_ns <= match['ns'] <= ns <= measured_end_ns:
                        if not ambiguous:
                            sample.add(ns - match['ns'])
                        if kind != 'pointer':
                            # Guaranteed FIFO edges have an ordinal identity even when
                            # identical pointer positions make the combined metric ambiguous.
                            discrete_sample.add(ns - match['ns'])
                        if kind == 'pointer':
                            applied_times.append(ns)
        result['unacknowledged_batch_count'] = len(pending)
        result['inject_error_count'] += source_errors
        result['unobserved_source_event_count'] = sum(not x['seen'] for x in measured)
        result['coalesced_or_dropped_pointer_count'] = sum(not x['seen'] and x['kind'] == 'pointer' for x in measured)
        result['missing_discrete_count'] = sum(not x['seen'] and x['kind'] != 'pointer' for x in measured)
        # Only intervals with actual source activity contribute to stall duration.
        start, end = max(measured_start_ns, measured[0]['ns']), min(measured_end_ns, measured[-1]['ns'])
        boundaries = [start] + sorted(set(t for t in applied_times if start < t < end)) + [end]
        gaps = [b - a for a, b in zip(boundaries, boundaries[1:])]
        threshold = stall_threshold_ms * 1e6
        result['stalls_over_50ms_count'] = sum(g > threshold for g in gaps)
        result['stall_excess_ms'] = sum(max(0, g - threshold) for g in gaps) / 1e6
        result['max_active_gap_ms'] = max(gaps, default=0) / 1e6
        result['final_position_delivered'] = any(
            lid == final['lease'] and ns >= final['ns'] and close_point(value, final['position'], pointer_tolerance)
            for ns, lid, value in applied_positions)
        result['pub_to_apply'] = sample.report()
        result['pub_to_apply_discrete'] = discrete_sample.report()
        failures = (result['wrong_click_count'] or result['wrong_order_count'] or result['lease_leak_count']
                    or result['nonmonotonic_batch_count'] or result['inject_error_count']
                    or result['missing_discrete_count'] or result['unmatched_receiver_event_count']
                    or result['unacknowledged_batch_count'] or not result['final_position_delivered'])
        result['status'] = 'fail' if failures else ('ok' if sample.count else 'unavailable')
        result['reason'] = ('oracle violation or incomplete delivery' if failures else
                            (None if sample.count else 'no matched measured completions'))
        return result
    except (OSError, ValueError, TypeError, struct.error, OverflowError, StopIteration) as error:
        # Do not echo exception strings: paths and protobuf input are private.
        result.update(status='aborted', reason='observer stream or oracle state invalid',
                      error_type=type(error).__name__)
        return result
