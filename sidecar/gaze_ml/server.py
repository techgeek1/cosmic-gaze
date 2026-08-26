"""A newline-delimited JSON broadcast server over a Unix stream socket.

The capture loop must never block on a consumer, so each client gets its own
bounded queue and its own writer thread. When a client's queue is full the
*oldest* record is dropped, not the newest: for a gaze stream a stale sample is
worthless, so a slow client should fall behind in age, not in freshness.
"""

from __future__ import annotations

import contextlib
import os
import queue
import socket
import threading
from dataclasses import dataclass, field
from pathlib import Path

# --- types ---


@dataclass
class ClientStats:
    """Per-client bookkeeping, reported in the periodic stats line."""

    sent:    int = 0
    dropped: int = 0


@dataclass
class _Client:
    """One connected consumer and the thread that feeds it."""

    sock:    socket.socket
    q:       queue.Queue
    stats:   ClientStats = field(default_factory=ClientStats)
    thread:  threading.Thread | None = None
    stop:    threading.Event = field(default_factory=threading.Event)


# --- the server ---


class BroadcastServer:
    """Accepts clients on a Unix socket and fans out one line per frame."""

    def __init__(self, path: str | Path, queue_depth: int = 8, backlog: int = 8) -> None:
        """Bind and listen. A stale socket file at `path` is removed first."""
        self.path        = Path(path)
        self.queue_depth = int(queue_depth)
        self._clients:   list[_Client] = []
        self._lock       = threading.Lock()
        self._stop       = threading.Event()

        self.path.parent.mkdir(parents=True, exist_ok=True)
        if self.path.exists() or self.path.is_socket():
            self.path.unlink()

        self._sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self._sock.bind(str(self.path))
        os.chmod(self.path, 0o600)
        self._sock.listen(backlog)
        self._sock.settimeout(0.5)

        self._accept_thread = threading.Thread(
            target=self._accept_loop, name="gaze-ml-accept", daemon=True
        )
        self._accept_thread.start()

    # --- accepting ---

    def _accept_loop(self) -> None:
        """Accept connections until `close`, spawning a writer thread per client."""
        while not self._stop.is_set():
            try:
                conn, _ = self._sock.accept()
            except TimeoutError:
                continue
            except OSError:
                break
            conn.setblocking(True)
            client = _Client(sock=conn, q=queue.Queue(maxsize=self.queue_depth))
            client.thread = threading.Thread(
                target=self._write_loop, args=(client,), name="gaze-ml-client", daemon=True
            )
            with self._lock:
                self._clients.append(client)
            client.thread.start()

    def _write_loop(self, client: _Client) -> None:
        """Drain one client's queue onto its socket until it dies or we stop."""
        try:
            while not self._stop.is_set() and not client.stop.is_set():
                try:
                    line = client.q.get(timeout=0.5)
                except queue.Empty:
                    continue
                if line is None:
                    break
                client.sock.sendall(line)
                client.stats.sent += 1
        except (BrokenPipeError, ConnectionResetError, OSError):
            pass
        finally:
            self._drop(client)

    # --- broadcasting ---

    def broadcast(self, line: bytes) -> None:
        """Enqueue one encoded line for every client. Never blocks."""
        with self._lock:
            clients = list(self._clients)
        for client in clients:
            try:
                client.q.put_nowait(line)
            except queue.Full:
                try:
                    client.q.get_nowait()
                    client.stats.dropped += 1
                except queue.Empty:
                    pass
                try:
                    client.q.put_nowait(line)
                except queue.Full:
                    client.stats.dropped += 1

    # --- lifecycle ---

    def _drop(self, client: _Client) -> None:
        """Remove a client and close its socket, idempotently."""
        with self._lock:
            if client in self._clients:
                self._clients.remove(client)
        client.stop.set()
        with contextlib.suppress(OSError):
            client.sock.close()

    @property
    def n_clients(self) -> int:
        """How many consumers are currently connected."""
        with self._lock:
            return len(self._clients)

    @property
    def dropped(self) -> int:
        """Total records dropped across all live clients."""
        with self._lock:
            return sum(c.stats.dropped for c in self._clients)

    def close(self) -> None:
        """Stop accepting, disconnect clients, and unlink the socket file."""
        self._stop.set()
        with contextlib.suppress(OSError):
            self._sock.close()
        with self._lock:
            clients = list(self._clients)
        for client in clients:
            self._drop(client)
        if self.path.exists() or self.path.is_socket():
            with contextlib.suppress(OSError):
                self.path.unlink()

    def __enter__(self) -> BroadcastServer:
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()
