"""Synthetic wire tests; no product executable, sockets, or native input."""
import importlib.util
from pathlib import Path
import struct
import tempfile
import unittest

_spec = importlib.util.spec_from_file_location('bench_observe', Path(__file__).with_name('observe.py'))
observe = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(observe)


def vi(value):
    out = bytearray()
    while value > 127:
        out.append((value & 127) | 128)
        value >>= 7
    out.append(value)
    return bytes(out)


def scalar(number, value):
    return vi(number << 3) + vi(value)


def blob(number, value):
    return vi(number << 3 | 2) + vi(len(value)) + value


def point(x, y):
    return blob(10, vi(2 << 3 | 1) + struct.pack('<d', x) + vi(3 << 3 | 1) + struct.pack('<d', y))


def button():
    return blob(12, scalar(1, 1) + scalar(2, 1))


def record(ns, direction, tag, body=b''):
    return observe.HEADER.pack(b'BBO1', direction, tag, 0, ns, 1, 123, len(body)) + body


class OracleTests(unittest.TestCase):
    def fixture(self, directory, *, wrong_click=False, release=False, partial=False):
        lease = b'L' * 16
        lid = blob(1, lease)
        source = (record(1, 0, 11, scalar(1, 3) + blob(2, lease))
                  + record(2, 1, 41, lid + scalar(2, 3))
                  + record(100_000_000, 1, 42, blob(1, point(961, 200)))
                  + record(110_000_000, 1, 42, blob(1, button())))
        receiver = record(3, 0, 13, lid) + record(3, 1, 40) + record(4, 0, 15, lid + scalar(2, 1000))
        p = point(960 if wrong_click else 961, 200)
        receiver += record(120_000_000, 0, 14, lid + scalar(2, 1) + blob(3, p))
        receiver += record(121_000_000, 1, 44, lid + scalar(2, 1) + scalar(3, 0 if partial else 1))
        if release:
            receiver += record(122_000_000, 0, 16, lid)
            receiver += record(123_000_000, 1, 46, lid)
        receiver += record(130_000_000, 0, 14, lid + scalar(2, 2) + blob(3, button()))
        receiver += record(131_000_000, 1, 44, lid + scalar(2, 2) + scalar(3, 1))
        a, b = Path(directory) / 'a.obs', Path(directory) / 'b.obs'
        a.write_bytes(source)
        b.write_bytes(receiver)
        return a, b

    def test_successful_correlated_completion(self):
        with tempfile.TemporaryDirectory() as directory:
            paths = self.fixture(directory)
            result = observe.analyze(*paths, 50_000_000, 200_000_000)
            self.assertEqual(result['status'], 'ok')
            self.assertEqual(result['pub_to_apply']['count'], 2)
            self.assertEqual(result['pub_to_apply']['p50_ms'], 21)
            self.assertEqual(result['pub_to_apply_discrete']['count'], 1)
            self.assertEqual(result['pub_to_apply_discrete']['p95_ms'], 21)
            self.assertTrue(result['final_position_delivered'])
            self.assertNotIn('position', result)

    def test_wrong_click_is_not_a_pass(self):
        with tempfile.TemporaryDirectory() as directory:
            paths = self.fixture(directory, wrong_click=True)
            result = observe.analyze(*paths, 50_000_000, 200_000_000)
            self.assertEqual(result['status'], 'fail')
            self.assertEqual(result['wrong_click_count'], 1)

    def test_release_blocks_later_application(self):
        with tempfile.TemporaryDirectory() as directory:
            paths = self.fixture(directory, release=True)
            result = observe.analyze(*paths, 50_000_000, 200_000_000)
            self.assertEqual(result['status'], 'fail')
            self.assertGreater(result['lease_leak_count'], 0)

    def test_partial_result_is_not_latency_success(self):
        with tempfile.TemporaryDirectory() as directory:
            paths = self.fixture(directory, partial=True)
            result = observe.analyze(*paths, 50_000_000, 200_000_000)
            self.assertEqual(result['status'], 'fail')
            self.assertEqual(result['pub_to_apply']['count'], 1)

    def test_truncated_stream_aborts(self):
        with tempfile.TemporaryDirectory() as directory:
            a, b = self.fixture(directory)
            b.write_bytes(b.read_bytes()[:-1])
            result = observe.analyze(a, b, 50_000_000, 200_000_000)
            self.assertEqual(result['status'], 'aborted')

    def test_delayed_repeated_positions_do_not_invent_reordering_or_latency(self):
        with tempfile.TemporaryDirectory() as directory:
            lease = b'L' * 16
            lid = blob(1, lease)
            source = record(1, 0, 11, scalar(1, 3) + blob(2, lease))
            source += record(2, 1, 41, lid + scalar(2, 3))
            for at, x in [(100, 961), (110, 962), (120, 961), (130, 962), (140, 961)]:
                source += record(at * 1_000_000, 1, 42, blob(1, point(x, 200)))
            receiver = record(3, 0, 13, lid) + record(3, 1, 40) + record(4, 0, 15, lid + scalar(2, 1000))
            for batch, at, x in [(1, 125, 961), (2, 126, 962), (3, 145, 961)]:
                receiver += record(at * 1_000_000, 0, 14, lid + scalar(2, batch) + blob(3, point(x, 200)))
                receiver += record((at + 1) * 1_000_000, 1, 44, lid + scalar(2, batch) + scalar(3, 1))
            a, b = Path(directory) / 'a.obs', Path(directory) / 'b.obs'
            a.write_bytes(source)
            b.write_bytes(receiver)
            result = observe.analyze(a, b, 50_000_000, 200_000_000)
            self.assertEqual(result['wrong_order_count'], 0)
            self.assertEqual(result['ambiguous_pointer_match_count'], 2)
            self.assertEqual(result['pub_to_apply']['count'], 1)
            self.assertTrue(result['final_position_delivered'])

    def test_read_ahead_release_waits_for_actual_released_reply(self):
        with tempfile.TemporaryDirectory() as directory:
            lease = b'L' * 16
            lid = blob(1, lease)
            source = record(1, 0, 11, scalar(1, 3) + blob(2, lease))
            source += record(2, 1, 41, lid + scalar(2, 3))
            source += record(100_000_000, 1, 42, blob(1, point(961, 200)))
            receiver = record(3, 0, 13, lid) + record(3, 1, 40)
            receiver += record(4, 0, 15, lid + scalar(2, 1000))
            receiver += record(120_000_000, 0, 14, lid + scalar(2, 1) + blob(3, point(961, 200)))
            receiver += record(120_500_000, 0, 16, lid)
            receiver += record(121_000_000, 1, 44, lid + scalar(2, 1) + scalar(3, 1))
            receiver += record(122_000_000, 1, 46, lid)
            a, b = Path(directory) / 'a.obs', Path(directory) / 'b.obs'
            a.write_bytes(source)
            b.write_bytes(receiver)
            result = observe.analyze(a, b, 50_000_000, 200_000_000)
            self.assertEqual(result['status'], 'ok')
            self.assertEqual(result['lease_leak_count'], 0)
            self.assertEqual(result['pub_to_apply']['count'], 1)

    def test_varint_overflow_rejected(self):
        with self.assertRaises(ValueError):
            observe.fields(b'\x08' + b'\xff' * 10)


if __name__ == '__main__':
    unittest.main()
