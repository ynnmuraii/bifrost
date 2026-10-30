"""Real two-core dry-input fixture, shared unchanged by before/after binaries.

Only run inside the isolated benchmark container. No native input is enabled.
Pairing output (including SAS) stays in memory, never in benchmark reports.
"""

import errno
import math
import os
from pathlib import Path
import re
import shlex
import socket
import subprocess
import threading
import time
from enum import Enum


class Side(str, Enum):
    BEFORE = "before"
    AFTER = "after"

    @property
    def env_prefix(self):
        return "LWS_" if self is Side.BEFORE else "BIFROST_"


def _wait(predicate, timeout, description):
    deadline = time.monotonic() + timeout
    while True:
        result = predicate()
        if result:
            return result
        if time.monotonic() >= deadline:
            raise TimeoutError(description)
        time.sleep(0.02)


def _publish(path, text):
    staging = path.with_name(path.name + ".writing")
    staging.write_text(text, encoding="utf-8")
    staging.replace(path)


def _text(path):
    try:
        return path.read_text(encoding="utf-8")
    except FileNotFoundError:
        return ""


def _port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


class Node:
    def __init__(self, executable, side, root, label, port, clipboard_port,
                 observer_preload=None, observer_path=None, can_own=False,
                 claim_on_startup=False):
        self.executable = str(executable)
        self.side = Side(side)
        self.env_prefix = self.side.env_prefix
        self.root = Path(root)
        self.label = label
        self.port = port
        self.clipboard_port = clipboard_port
        self.can_own = can_own
        self.claim_on_startup = claim_on_startup
        self.control_dir = self.root / "run/control-dry"
        self.emergency_path = self.root / "run/emergency-dry"
        self.log_path = self.root / "core.log"
        agent_name = "lws-agent.log" if self.side is Side.BEFORE else "bifrost-agent.log"
        self.agent_log_path = self.root / "state/logs" / agent_name
        self.observer_preload = str(observer_preload) if observer_preload else None
        self.observer_path = Path(observer_path) if observer_path else None
        self.id = None
        self.process = None

    @property
    def pid(self):
        return self.process.pid if self.process is not None else None

    @property
    def agent_pid(self):
        self.check_alive()
        children = Path(f"/proc/{self.pid}/task/{self.pid}/children").read_text().split()
        agents = []
        for child in children:
            try:
                argv = Path(f"/proc/{child}/cmdline").read_bytes().split(b"\0")
            except FileNotFoundError:
                continue
            if b"agent" in argv:
                agents.append(int(child))
        if len(agents) != 1:
            raise RuntimeError("expected exactly one real agent child")
        return agents[0]

    def command(self, *args):
        return [self.executable, "--root", str(self.root), *map(str, args)]

    def prepare(self):
        if self.root.exists() and any(self.root.iterdir()):
            raise FileExistsError("benchmark node requires a fresh root: " + str(self.root))
        for directory in ("config", "data", "state", "run/control-dry"):
            (self.root / directory).mkdir(parents=True, exist_ok=True)
        boolean = lambda value: "true" if value else "false"
        (self.root / "config/config.toml").write_text(f'''config_version = 2
[node]
display_name = "BENCH-{self.label}"
can_own = {boolean(self.can_own)}
claim_on_startup = {boolean(self.claim_on_startup)}
[network]
listen_address = "127.0.0.1"
port_input = {self.port}
port_clipboard = {self.clipboard_port}
max_members = 3
[discovery]
enabled = false
advertise = false
[clipboard]
enabled = false
[timing]
lease_ttl_ms = 1000
[input]
require_emergency_hotkey = false
pointer_gain = 1.0
edge_dwell_ms = 0
edge_hysteresis_px = 8
takeover_threshold_px = 12
repeat_mode = "receiver_software"
takeover_detection = "off"
[log]
level = "info"
''', encoding="utf-8")

    def add_topology(self, peer_id):
        # The stable v2 display format is accepted by both schemas; no edge links.
        with (self.root / "config/config.toml").open("a", encoding="utf-8") as stream:
            stream.write(f'''
[topology]
revision = 1
author = "self"
max_seen_revision = 1
max_seen_author = "self"
[[topology.displays]]
node = "self"
id = "dry-0"
logical = [0.0, 0.0, 1920.0, 1080.0]
primary = true
[[topology.displays]]
node = "peer:{peer_id}"
id = "dry-0"
logical = [0.0, 0.0, 1920.0, 1080.0]
primary = true
''')

    def start(self):
        if self.process is not None:
            raise RuntimeError("node already started")
        env = os.environ.copy()
        # Do not inherit an unrelated dry mode, logger override or preload.
        for key in list(env):
            if key.startswith(("LWS_DRY_", "BIFROST_DRY_")) or key in (
                    "LD_PRELOAD", "BF_BENCH_OBSERVE", "LWS_LOG", "BIFROST_LOG"):
                del env[key]
        env[self.env_prefix + "DRY_CONTROL_DIR"] = str(self.control_dir)
        env[self.env_prefix + "DRY_EMERGENCY_FILE"] = str(self.emergency_path)
        if self.observer_preload:
            if self.observer_path is None:
                raise ValueError("observer preload requires observer_path")
            if self.observer_path.exists():
                raise FileExistsError("observer output must be fresh")
            self.observer_path.parent.mkdir(parents=True, exist_ok=True)
            env["LD_PRELOAD"] = self.observer_preload
            env["BF_BENCH_OBSERVE"] = str(self.observer_path)
        with self.log_path.open("wb") as log:
            self.process = subprocess.Popen(
                self.command("run", "--dry-inject", "--dry-input-events", "0", "--dry-displays"),
                env=env, stdin=subprocess.DEVNULL, stdout=log, stderr=log)

    def check_alive(self):
        if self.process is None or self.process.poll() is not None:
            raise RuntimeError("benchmark core not running; inspect " + str(self.log_path))

    def ctl(self, *request, timeout=5):
        if len(request) == 1:
            request = shlex.split(request[0])
        output = subprocess.run(self.command(*request), stdin=subprocess.DEVNULL,
                                capture_output=True, text=True, timeout=timeout)
        reply = output.stdout.strip()
        if output.returncode or not (reply == "OK" or reply.startswith("OK ")):
            raise RuntimeError("control request failed: " + (reply or output.stderr.strip()))
        return reply

    def status(self):
        reply = self.ctl("status")
        fields = {}
        current = None
        for token in reply[3:].split():
            if "=" in token:
                current, value = token.split("=", 1)
                fields[current] = value
            elif current:
                fields[current] += " " + token
            else:
                raise ValueError("invalid status field")
        required = {"focus", "peer", "members", "agent", "node", "owner", "pressed"}
        if not required.issubset(fields):
            raise ValueError("incomplete status reply")
        for key in ("members", "pressed", "serial"):
            if key in fields:
                fields[key] = int(fields[key])
        return fields

    def capture_mode(self):
        return _text(self.control_dir / "capture-mode.txt").strip()

    def hold_pointer(self, x=960, y=540, timeout=10):
        if not all(math.isfinite(value) for value in (x, y)):
            raise ValueError("pointer coordinates must be finite")
        sample = f"{x} {y}"
        observed = self.control_dir / "pointer-observed.txt"
        observed.unlink(missing_ok=True)
        _publish(self.control_dir / "held-pointer.txt", sample)
        def acknowledged():
            self.check_alive()
            return _text(observed) == sample
        _wait(acknowledged, timeout, "dry pointer sample was not observed")

    def release_pointer(self):
        (self.control_dir / "held-pointer.txt").unlink(missing_ok=True)

    def click(self):
        path = self.control_dir / "click"
        if path.exists():
            raise RuntimeError("previous dry click has not been consumed")
        _publish(path, "click")

    def wheel(self, detents):
        if not isinstance(detents, int) or not -2147483648 < detents <= 2147483647:
            raise ValueError("wheel detents must fit signed 32 bits, excluding MIN")
        path = self.control_dir / "wheel"
        if path.exists():
            raise RuntimeError("previous dry wheel has not been consumed")
        _publish(path, str(detents))

    def trigger_emergency(self):
        _publish(self.emergency_path, "emergency")

    def stop(self):
        if self.process is None or self.process.poll() is not None:
            return
        try:
            self.ctl("quit")
        except (RuntimeError, subprocess.TimeoutExpired):
            self.process.terminate()
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=5)


class TcpProxy:
    """Two listeners preserve TLS and cover either winning deduplicated link.

    pause() prevents recv on every source-to-receiver pump, not merely send.
    It returns after in-flight recv calls leave their read sections.
    No bytes are inspected, retained on disk, or included in errors.
    """

    def __init__(self, source_port, receiver_port):
        self._stop = threading.Event()
        self._paused = False
        self._condition = threading.Condition()
        self._reading = 0
        self._sockets = []
        self._threads = []
        self._errors = []
        self.source_address = self._listen(source_port, False)
        self.receiver_address = self._listen(receiver_port, True)

    @staticmethod
    def _tune(sock):
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 4096)
        sock.settimeout(0.05)

    def _thread(self, function, *args):
        thread = threading.Thread(target=function, args=args, daemon=True)
        self._threads.append(thread)
        thread.start()

    def _listen(self, target_port, toward_receiver):
        listener = socket.socket()
        self._tune(listener)
        listener.bind(("127.0.0.1", 0))
        listener.listen()
        self._sockets.append(listener)
        self._thread(self._accept, listener, target_port, toward_receiver)
        return "127.0.0.1:" + str(listener.getsockname()[1])

    def _accept(self, listener, target_port, toward_receiver):
        while not self._stop.is_set():
            try:
                incoming, _ = listener.accept()
            except socket.timeout:
                continue
            except OSError as error:
                if not self._stop.is_set():
                    self._errors.append(error.errno)
                return
            self._tune(incoming)
            outgoing = socket.socket()
            self._tune(outgoing)
            self._sockets.extend((incoming, outgoing))
            try:
                outgoing.settimeout(2)
                outgoing.connect(("127.0.0.1", target_port))
                outgoing.settimeout(0.05)
            except OSError as error:
                # Initial dialing may race the other core's listener; the real
                # transport reconnects. wait_linked still requires a real link.
                if error.errno != errno.ECONNREFUSED and not self._stop.is_set():
                    self._errors.append(error.errno)
                incoming.close()
                outgoing.close()
                continue
            self._thread(self._pump, incoming, outgoing, toward_receiver)
            self._thread(self._pump, outgoing, incoming, not toward_receiver)

    def _pump(self, reader, writer, source_to_receiver):
        try:
            while not self._stop.is_set():
                if source_to_receiver:
                    with self._condition:
                        self._condition.wait_for(lambda: not self._paused or self._stop.is_set())
                        if self._stop.is_set():
                            return
                        self._reading += 1
                try:
                    try:
                        data = reader.recv(4096)
                    except socket.timeout:
                        continue
                    if not data:
                        return
                finally:
                    if source_to_receiver:
                        with self._condition:
                            self._reading -= 1
                            self._condition.notify_all()
                pending = memoryview(data)
                while pending and not self._stop.is_set():
                    try:
                        sent = writer.send(pending)
                    except socket.timeout:
                        continue
                    if not sent:
                        return
                    pending = pending[sent:]
        except OSError as error:
            if not self._stop.is_set() and error.errno not in (
                    errno.ECONNRESET, errno.EPIPE, errno.ENOTCONN):
                self._errors.append(error.errno)
        finally:
            try:
                writer.shutdown(socket.SHUT_WR)
            except OSError:
                pass

    def pause(self, timeout=2):
        with self._condition:
            self._paused = True
            if not self._condition.wait_for(lambda: self._reading == 0, timeout):
                raise TimeoutError("proxy recv did not pause")
        self.assert_healthy()
        return time.monotonic_ns()

    def resume(self):
        with self._condition:
            self._paused = False
            self._condition.notify_all()
        self.assert_healthy()
        return time.monotonic_ns()

    def assert_healthy(self):
        if self._errors:
            raise RuntimeError("TCP fixture proxy socket errors: " + str(self._errors))

    def stop(self):
        self._stop.set()
        with self._condition:
            self._condition.notify_all()
        for sock in list(self._sockets):
            sock.close()
        for thread in list(self._threads):
            thread.join(timeout=3)


class Lab:
    def __init__(self, executable, side, root="/tmp/bb", observer_preload=None,
                 observer_paths=None, proxy=False, receiver_can_own=False):
        self.side = Side(side)
        self.root = Path(root)
        ports = set()
        while len(ports) < 4:
            ports.add(_port())
        a_port, b_port, a_clip, b_clip = sorted(ports)
        paths = observer_paths or (self.root / "a.obs", self.root / "b.obs")
        if len(paths) != 2:
            raise ValueError("observer_paths must contain source and receiver paths")
        self.source = Node(executable, self.side, self.root / "a", "SOURCE", a_port,
                           a_clip, observer_preload, paths[0], True, True)
        self.receiver = Node(executable, self.side, self.root / "b", "RECEIVER", b_port,
                             b_clip, observer_preload, paths[1], receiver_can_own, False)
        self.use_proxy = proxy
        self.proxy = None
        self._paired = False

    def prepare(self):
        self.source.prepare()
        self.receiver.prepare()
        return self

    def pair(self, timeout=45):
        a, b = self.source, self.receiver
        opened = threading.Event()
        opener_text = []
        opener = subprocess.Popen(b.command("pair", "--open", "--listen", f"127.0.0.1:{b.port}"),
                                  stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  stderr=subprocess.DEVNULL, text=True)
        def read_opener():
            for line in opener.stdout:
                opener_text.append(line)
                if "waiting for a peer to dial" in line:
                    opened.set()
        reader = threading.Thread(target=read_opener, daemon=True)
        reader.start()
        try:
            opener.stdin.write("y\n")
            opener.stdin.close()
            if not opened.wait(timeout):
                raise TimeoutError("pair opener did not announce its waiting window")
            dialer = subprocess.run(a.command("pair", "--dial", f"127.0.0.1:{b.port}"),
                                    input="y\n", capture_output=True, text=True, timeout=timeout)
            code = opener.wait(timeout=timeout)
            reader.join(timeout=2)
            if code or dialer.returncode or "paired with" not in dialer.stdout or not any(
                    "paired with" in line for line in opener_text):
                raise RuntimeError("real CLI pairing failed; SAS output intentionally not logged")
        finally:
            if opener.poll() is None:
                opener.kill()
                opener.wait(timeout=5)
            reader.join(timeout=2)
            opener.stdout.close()
        # The other root's real trust record supplies each node's identity.
        a.id = self._trusted_id(b, a.port)
        b.id = self._trusted_id(a, b.port)
        a.add_topology(b.id)
        b.add_topology(a.id)
        if self.use_proxy:
            self.proxy = TcpProxy(a.port, b.port)
            self._retarget(a, self.proxy.receiver_address)
            self._retarget(b, self.proxy.source_address)
        self._paired = True
        return self

    @staticmethod
    def _trusted_id(node, expected_port):
        document = (node.root / "config/trust.toml").read_text(encoding="utf-8")
        ids = re.findall(r'^\s*node_id\s*=\s*"([0-9a-fA-F]{64})"\s*$', document, re.M)
        addresses = re.findall(r'^\s*last_address\s*=\s*"([^"\n]+)"\s*$', document, re.M)
        if len(ids) != 1 or addresses != [f"127.0.0.1:{expected_port}"]:
            raise RuntimeError("pairing did not produce one real peer with its actual listen port")
        return ids[0].lower()

    @staticmethod
    def _retarget(node, address):
        path = node.root / "config/trust.toml"
        document = path.read_text(encoding="utf-8")
        document, count = re.subn(r'^\s*last_address\s*=.*$',
                                  f'last_address = "{address}"', document, flags=re.M)
        if count != 1:
            raise RuntimeError("proxy requires exactly one existing trusted peer")
        _publish(path, document)

    def start(self):
        if not self._paired:
            raise RuntimeError("prepare and real pair must precede start")
        try:
            self.source.start()
            self.receiver.start()
        except BaseException:
            self.stop()
            raise
        return self

    def status(self):
        return {"source": self.source.status(), "receiver": self.receiver.status()}

    def wait_linked(self, timeout=30):
        def linked():
            self.source.check_alive()
            self.receiver.check_alive()
            if self.proxy:
                self.proxy.assert_healthy()
            try:
                statuses = self.status()
            except (RuntimeError, subprocess.TimeoutExpired):
                return False
            return statuses if all(s["members"] == 1 and s["agent"] == "ready"
                                   and s["owner"] == self.source.id
                                   for s in statuses.values()) else False
        return _wait(linked, timeout, "cores did not link with ready agents and source ownership")

    def lease_active(self):
        statuses = self.status()
        return (statuses["source"]["focus"] == "Remote"
                and statuses["source"]["peer"] == self.receiver.id
                and statuses["receiver"]["peer"] == self.source.id)

    def switch_remote(self, timeout=15):
        self.wait_linked(timeout)
        self.source.hold_pointer(timeout=timeout)
        self.source.ctl("switch", "--to", self.receiver.id)
        def active():
            self.source.check_alive()
            self.receiver.check_alive()
            return self.lease_active() and self.source.capture_mode() == "Remote"
        _wait(active, timeout, "manual switch did not establish the real remote lease")
        return self.status()

    def stop(self):
        try:
            self.source.stop()
        finally:
            try:
                self.receiver.stop()
            finally:
                if self.proxy:
                    self.proxy.stop()

    def __enter__(self):
        try:
            return self.prepare().pair().start()
        except BaseException:
            self.stop()
            raise

    def __exit__(self, *exc):
        self.stop()
