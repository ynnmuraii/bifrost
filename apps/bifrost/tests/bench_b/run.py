#!/usr/bin/env python3
"""Bench-only Linux runner: two real cores/dry agents and container-local tc netem.

Run under Xvfb in the dedicated NET_ADMIN container, never on a host interface.
The identical external fixture is used for both release and working-tree builds.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import time

from nodes import Lab
from observe import analyze


def netem(loss=0, delay=0, jitter=0, seed=17):
    command = ["tc", "qdisc", "replace", "dev", "lo", "root", "netem", "limit", "10000"]
    if delay or jitter:
        command += ["delay", f"{delay}ms", f"{jitter}ms", "distribution", "normal"]
    command += ["loss", "random", f"{loss}%", "seed", str(seed)]
    subprocess.run(command, check=True, capture_output=True)


def clear_netem():
    # This container's isolated loopback only. A missing root qdisc is harmless.
    result = subprocess.run(["tc", "qdisc", "del", "dev", "lo", "root"], capture_output=True)
    if result.returncode and b"No such file" not in result.stderr and b"Cannot delete qdisc with handle of zero" not in result.stderr:
        raise RuntimeError("cannot clear fixture loopback qdisc")


def tcp_counters():
    """Namespace-local counters only; no packet payloads, addresses or host qdisc."""
    result = {}
    for path, section, wanted in (
        ("/proc/net/snmp", "Tcp", ("InSegs", "OutSegs", "RetransSegs")),
        ("/proc/net/netstat", "TcpExt", ("TCPTimeouts", "TCPFastRetrans", "TCPLossProbes")),
    ):
        lines = Path(path).read_text().splitlines()
        for index, line in enumerate(lines[:-1]):
            if line.startswith(section + ":"):
                values = dict(zip(line.split()[1:], map(int, lines[index + 1].split()[1:]), strict=True))
                result.update({section + "." + name: values[name] for name in wanted})
                break
    return result


def qdisc_counters():
    entries = json.loads(subprocess.check_output(["tc", "-s", "-j", "qdisc", "show", "dev", "lo"]))
    for entry in entries:
        if entry["kind"] == "netem":
            return {name: entry[name] for name in ("bytes", "packets", "drops")}
    return None


def publish_pointer(node, x, y):
    target = node.control_dir / "held-pointer.txt"
    staging = target.with_name("held-pointer.bench-writing")
    staging.write_text(f"{x:.6f} {y:.6f}", encoding="ascii")
    staging.replace(target)
    return f"{x:.6f} {y:.6f}"


def exercise(lab, seconds, fault=None):
    """Wall-clock paced, unique-y synthetic samples; no source wait hides stalls."""
    tcp_before = tcp_counters()
    start = time.monotonic_ns()
    deadline = start + int(seconds * 1e9)
    next_tick = start
    generated = clicks = 0
    fault_started = fault_cleared = returned = None
    mode_before = lab.source.capture_mode()
    while time.monotonic_ns() < deadline:
        now = time.monotonic_ns()
        elapsed = (now - start) / 1e9
        if fault and fault_started is None and elapsed >= 0.5:
            if fault == "stalled_reader":
                fault_started = lab.proxy.pause()
            else:
                netem(loss=100)
                fault_started = time.monotonic_ns()
        if fault_started is not None and fault_cleared is None:
            duration = 2.0 if fault == "stalled_reader" else 0.5
            if (now - fault_started) / 1e9 >= duration:
                if fault == "stalled_reader":
                    fault_cleared = lab.proxy.resume()
                else:
                    clear_netem()
                    fault_cleared = time.monotonic_ns()
        final_text = publish_pointer(lab.source, 960.0, 200.0 + generated * 0.1)
        generated += 1
        if generated % 25 == 0:
            lab.source.click()
            clicks += 1
        if fault_started is not None and returned is None:
            mode = lab.source.capture_mode()
            if mode and mode != "Remote":
                returned = time.monotonic_ns()
        next_tick += 10_000_000
        sleep = min(next_tick, deadline) - time.monotonic_ns()
        if sleep > 0:
            time.sleep(sleep / 1e9)
    end = time.monotonic_ns()
    observed_final = False
    observed_deadline = time.monotonic() + 0.5
    while time.monotonic() < observed_deadline:
        try:
            observed_final = (lab.source.control_dir / "pointer-observed.txt").read_text() == final_text
        except FileNotFoundError:
            pass
        if observed_final:
            break
        time.sleep(0.005)
    lab.source.release_pointer()
    if fault and fault_cleared is None and fault_started is not None:
        if fault == "stalled_reader":
            fault_cleared = lab.proxy.resume()
        else:
            clear_netem()
            fault_cleared = time.monotonic_ns()
    # Finish pending writes without creating a new sample to rescue a lost final point.
    drain_end = time.monotonic() + 2.0
    while time.monotonic() < drain_end:
        if fault_started is not None and returned is None:
            mode = lab.source.capture_mode()
            if mode and mode != "Remote":
                returned = time.monotonic_ns()
        time.sleep(0.01)
    tcp_after = tcp_counters()
    return {
        "measured_start_ns": start,
        "measured_end_ns": end,
        "delivery_drain_end_ns": time.monotonic_ns(),
        "generated_pointer_updates": generated,
        "final_update_observed_at_agent": observed_final,
        "generated_clicks": clicks,
        "source_remote_before": mode_before == "Remote",
        "source_remote_after": lab.source.capture_mode() == "Remote",
        "fault": fault,
        "fault_start_ns": fault_started,
        "fault_cleared_ns": fault_cleared,
        "control_return_us": (returned - fault_started) // 1000 if returned is not None else None,
        "control_return_within_ttl_plus_100": (returned - fault_started) <= 1_100_000_000 if returned is not None else None,
        "blackout_survived": returned is None if fault == "blackout_500ms" else None,
        "tcp_counter_scope": "measured interval plus delivery drain",
        "tcp_counters": {name: tcp_after[name] - value for name, value in tcp_before.items()},
        "netem_counters": qdisc_counters(),
    }


def recover_new_lease(lab):
    """Test that buffered old input cannot carry into a fresh, healthy lease."""
    clear_netem()
    if lab.source.capture_mode() == "Remote":
        lab.source.ctl("switch", "--to", "local")
    deadline = time.monotonic() + 5
    while lab.source.capture_mode() == "Remote" or lab.source.status()["focus"] != "Local":
        if time.monotonic() >= deadline:
            raise TimeoutError("source did not leave old lease")
        time.sleep(0.01)
    lab.wait_linked()
    lab.switch_remote()
    for index in range(10):
        publish_pointer(lab.source, 960.0, 700.0 + index * 0.1)
        time.sleep(0.01)
    lab.source.release_pointer()
    time.sleep(0.5)
    return lab.lease_active()


def after_metrics(node):
    """Retain only duration/counter metric records, not unrelated product logs."""
    lines = []
    for path in (node.log_path, node.agent_log_path):
        if path.exists():
            lines.extend(line for line in path.read_text(errors="replace").splitlines()
                         if "input duration metrics" in line or "input counter metrics" in line)
    return lines


def run_case(args, profile):
    side = args.side
    label = profile["case"]
    if label == "matrix":
        label += f'-l{profile["loss_pct"]}-d{profile["delay_ms"]}-j{profile["jitter_ms"]}-s{profile["seed"]}'
    output = Path(args.output)
    artifacts = output.parent / f"{side}-{label}"
    artifacts.mkdir(parents=True, exist_ok=False)
    root = Path(args.root)
    if root.exists():
        raise FileExistsError("bench root must be fresh")
    lab = Lab(args.binary, side, root=root, observer_preload=args.observer,
              proxy=profile["case"] == "stalled_reader")
    row = {"side": side, **profile, "ttl_ms": 1000}
    phase = "setup"
    window = None
    try:
        clear_netem()
        lab.prepare().pair().start()
        lab.wait_linked()
        lab.switch_remote()
        time.sleep(0.1)
        if profile["case"] == "matrix":
            netem(profile["loss_pct"], profile["delay_ms"], profile["jitter_ms"], profile["seed"])
        phase = "measure"
        duration = args.seconds if profile["case"] == "matrix" else 4.0
        window = exercise(lab, duration, None if profile["case"] == "matrix" else profile["case"])
        row.update({key: value for key, value in window.items() if not key.endswith("_ns")})
        (artifacts / "window.json").write_text(json.dumps(window), encoding="utf-8")
        if profile["case"] != "matrix":
            phase = "recovery"
            row["recovery_lease_active"] = recover_new_lease(lab)
        phase = "stop"
        lab.stop()
        phase = "oracle"
        oracle = analyze(lab.source.observer_path, lab.receiver.observer_path,
                         window["measured_start_ns"], window["measured_end_ns"],
                         final_source_before_ns=window["delivery_drain_end_ns"])
        row.update(oracle)
        if profile["case"] != "matrix":
            recovery = analyze(lab.source.observer_path, lab.receiver.observer_path,
                               window["measured_start_ns"], window["measured_end_ns"])
            row["recovery_final_position_delivered"] = recovery["final_position_delivered"]
        if profile["case"] == "stalled_reader":
            row["safety_acceptance"] = row["control_return_within_ttl_plus_100"] is True and not row["lease_leak_count"]
        else:
            row["safety_acceptance"] = not row["lease_leak_count"]
        if side == "after":
            (artifacts / "input-metrics.log").write_text(
                "\n".join(after_metrics(lab.source) + after_metrics(lab.receiver)) + "\n")
    except Exception as error:
        row.update(status="fixture_error", phase=phase, error_type=type(error).__name__)
        # Exception text is private diagnostic evidence, not public result data.
        (artifacts / "fixture-error.txt").write_text(str(error), encoding="utf-8")
    finally:
        try:
            clear_netem()
        finally:
            lab.stop()
        for node, prefix in ((lab.source, "source"), (lab.receiver, "receiver")):
            for path, suffix in ((node.log_path, "core.log"), (node.agent_log_path, "agent.log"),
                                 (node.observer_path, "obs")):
                if path.exists():
                    shutil.copyfile(path, artifacts / f"{prefix}.{suffix}")
        if root.exists():
            shutil.rmtree(root)
    with output.open("a", encoding="utf-8") as stream:
        stream.write(json.dumps(row, sort_keys=True) + "\n")
    print(json.dumps(row, sort_keys=True), flush=True)
    return row


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--side", choices=("before", "after"), required=True)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--observer", default="/fixture/libbf_bench_observer.so")
    parser.add_argument("--root", default="/tmp/bb")
    parser.add_argument("--output", required=True)
    parser.add_argument("--seconds", type=float, default=30)
    parser.add_argument("--seeds", default="17,23,41")
    parser.add_argument("--losses", default="0,1,5")
    parser.add_argument("--delays", default="0:0,10:5,30:10")
    parser.add_argument("--cases", default="matrix,stalled_reader,blackout_500ms")
    parser.add_argument("--stop-on-error", action="store_true")
    args = parser.parse_args()
    if not Path("/.dockerenv").exists():
        raise SystemExit("refusing to modify loopback qdisc outside Docker")
    if args.seconds <= 0 or args.seconds > 30:
        raise SystemExit("measured interval must be in (0,30] seconds")
    output = Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    if output.exists():
        raise SystemExit("refusing to overwrite existing benchmark results")
    cases = args.cases.split(",")
    profiles = []
    if "matrix" in cases:
        for loss in map(int, args.losses.split(",")):
            for delay, jitter in (map(int, pair.split(":")) for pair in args.delays.split(",")):
                for seed in map(int, args.seeds.split(",")):
                    profiles.append({"case": "matrix", "loss_pct": loss, "delay_ms": delay,
                                     "jitter_ms": jitter, "seed": seed})
    for case in ("stalled_reader", "blackout_500ms"):
        if case in cases:
            profiles.append({"case": case, "loss_pct": None, "delay_ms": 0,
                             "jitter_ms": 0, "seed": 17})
    for profile in profiles:
        row = run_case(args, profile)
        if args.stop_on_error and row["status"] in ("fixture_error", "aborted", "unavailable"):
            raise SystemExit(2)


if __name__ == "__main__":
    main()
