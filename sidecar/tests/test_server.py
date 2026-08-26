"""The broadcast socket: fan-out, per-client drops, and cleanup."""

from __future__ import annotations

import socket
import time

from gaze_ml import schema
from gaze_ml.server import BroadcastServer


def connect(server: BroadcastServer) -> socket.socket:
    """Connect one client and wait for the server to register it."""
    before = server.n_clients
    sock   = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.connect(str(server.path))
    deadline = time.monotonic() + 2.0
    while server.n_clients <= before and time.monotonic() < deadline:
        time.sleep(0.005)
    return sock


def read_lines(sock: socket.socket, count: int, timeout: float = 2.0) -> list[dict]:
    """Read exactly `count` newline-delimited records from a client socket."""
    sock.settimeout(timeout)
    buf: bytes = b""
    out: list[dict] = []
    while len(out) < count:
        buf += sock.recv(65536)
        while b"\n" in buf and len(out) < count:
            line, _, buf = buf.partition(b"\n")
            out.append(schema.decode(line))
    return out


def frame(seq: int) -> bytes:
    """One encoded invalid record, which is enough to exercise the transport."""
    return schema.encode(schema.record(t=float(seq), seq=seq, lat_ms=1.0, valid=False))


def test_socket_is_created_and_removed(tmp_path) -> None:
    """The socket file exists while serving and is unlinked on close."""
    path = tmp_path / "gaze.sock"
    with BroadcastServer(path) as server:
        assert server.path.is_socket()
    assert not path.exists()


def test_stale_socket_is_replaced(tmp_path) -> None:
    """A leftover file from a crashed run must not block the next bind."""
    path = tmp_path / "gaze.sock"
    path.write_text("stale")
    with BroadcastServer(path) as server:
        assert server.path.is_socket()


def test_every_client_gets_every_record(tmp_path) -> None:
    """Two consumers see the same stream."""
    with BroadcastServer(tmp_path / "gaze.sock") as server:
        a, b = connect(server), connect(server)
        assert server.n_clients == 2
        for seq in range(5):
            server.broadcast(frame(seq))
        assert [r["seq"] for r in read_lines(a, 5)] == list(range(5))
        assert [r["seq"] for r in read_lines(b, 5)] == list(range(5))
        a.close()
        b.close()


def test_broadcast_never_blocks_on_a_stalled_client(tmp_path) -> None:
    """A consumer that never reads must not slow the producer down."""
    with BroadcastServer(tmp_path / "gaze.sock", queue_depth=2) as server:
        stalled = connect(server)
        start   = time.monotonic()
        for seq in range(2000):
            server.broadcast(frame(seq))
        assert time.monotonic() - start < 2.0
        stalled.close()


def test_slow_client_drops_oldest_and_counts_them(tmp_path) -> None:
    """Overflow drops stale records, and the drop is accounted for, not hidden."""
    with BroadcastServer(tmp_path / "gaze.sock", queue_depth=2) as server:
        stalled = connect(server)
        for seq in range(500):
            server.broadcast(frame(seq))
        time.sleep(0.1)
        assert server.dropped > 0
        stalled.close()


def test_disconnected_client_is_reaped(tmp_path) -> None:
    """A vanished consumer is removed rather than raising on the next broadcast."""
    with BroadcastServer(tmp_path / "gaze.sock") as server:
        sock = connect(server)
        sock.close()
        deadline = time.monotonic() + 3.0
        while server.n_clients and time.monotonic() < deadline:
            server.broadcast(frame(0))
            time.sleep(0.02)
        assert server.n_clients == 0


def test_serving_with_no_clients_is_fine(tmp_path) -> None:
    """Broadcasting into the void is a no-op, not an error."""
    with BroadcastServer(tmp_path / "gaze.sock") as server:
        for seq in range(100):
            server.broadcast(frame(seq))
        assert server.n_clients == 0
