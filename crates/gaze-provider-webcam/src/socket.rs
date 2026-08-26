//! Reading newline-delimited records off a Unix socket without ever blocking longer than
//! the caller's stop check, and the reconnect backoff that goes with it.
//!
//! `BufRead::read_line` is not usable here: the socket carries a read timeout so the
//! reader thread can notice `stop()`, and a timeout mid-line leaves `read_line`'s buffer
//! in an unspecified state. `LineStream` keeps its own byte buffer instead, so a timeout
//! that lands in the middle of a JSON object costs nothing and the line completes on the
//! next poll.

use std::io::{ErrorKind, Read};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

/// Read chunk size. A 30 Hz sidecar line is a couple of hundred bytes, so this holds a
/// second of backlog in one syscall.
const CHUNK: usize = 8192;

/// Largest partial line tolerated before the buffer is dropped. A peer that never sends a
/// newline is broken, and the reader must not grow without bound waiting for it.
const MAX_LINE: usize = 1 << 20;

/// Shortest reconnect delay.
const MIN_BACKOFF: Duration = Duration::from_millis(100);

/// Longest reconnect delay. The sidecar can be down for a while (no camera plugged in);
/// two seconds keeps the log quiet without making a restart feel sluggish.
const MAX_BACKOFF: Duration = Duration::from_secs(2);

/// A connected Unix socket, split into lines as bytes arrive.
pub struct LineStream {
    stream  : UnixStream,
    /// Bytes read but not yet terminated by a newline.
    pending : Vec<u8>,
    /// Scratch for the current read, kept across calls so polling does not allocate.
    chunk   : Vec<u8>,
}

/// Exponential reconnect delay with a floor and a ceiling.
#[derive(Clone, Copy, Debug)]
pub struct Backoff {
    current : Duration,
}

// --- LineStream ---

impl LineStream {
    /// Connects to `path` and sets `read_timeout` so polling returns to the caller
    /// regularly even on a silent socket.
    pub fn connect(path: &Path, read_timeout: Duration) -> std::io::Result<Self> {
        let stream = UnixStream::connect(path)?;

        Self::from_stream(stream, read_timeout)
    }

    /// Wraps an already connected stream. Used by tests, which connect their own pair.
    pub fn from_stream(stream: UnixStream, read_timeout: Duration) -> std::io::Result<Self> {
        stream.set_read_timeout(Some(read_timeout))?;

        Ok(Self {
            stream  : stream,
            pending : Vec::new(),
            chunk   : vec![0; CHUNK],
        })
    }

    /// Reads whatever is available right now, appending every complete line to `out`
    /// (newline stripped, `\r` tolerated).
    ///
    /// Returns `Ok(true)` while the connection is alive, including when nothing arrived
    /// before the read timeout, and `Ok(false)` at clean end of stream. A real I/O error
    /// is reported as an error and means the same thing as end of stream to the caller:
    /// reconnect.
    pub fn poll(&mut self, out: &mut Vec<String>) -> std::io::Result<bool> {
        loop {
            match self.stream.read(&mut self.chunk) {
                Ok(0) => {
                    // Clean EOF. Anything left in `pending` was a truncated line, so it is
                    // dropped rather than parsed.
                    self.pending.clear();

                    return Ok(false);
                }

                Ok(n) => {
                    self.pending.extend_from_slice(&self.chunk[..n]);
                    self.drain_lines(out);

                    // A short read means the socket is drained; going round again would
                    // just block for the full timeout with nothing to show for it.
                    if n < self.chunk.len() {
                        return Ok(true);
                    }
                }

                // Both spellings show up for a socket read timeout depending on the
                // platform's errno, and neither means the peer went away.
                Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                    return Ok(true);
                }

                Err(e) if e.kind() == ErrorKind::Interrupted => continue,

                Err(e) => return Err(e),
            }
        }
    }
}

impl LineStream {
    /// Moves every newline-terminated line out of `pending` and into `out`.
    fn drain_lines(&mut self, out: &mut Vec<String>) {
        while let Some(idx) = self.pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=idx).collect();
            let text          = String::from_utf8_lossy(&line[..idx]);

            out.push(text.trim_end_matches('\r').to_string());
        }

        // A peer that never terminates a line must not be allowed to exhaust memory.
        if self.pending.len() > MAX_LINE {
            tracing::warn!("sidecar sent {} bytes with no newline; dropping the partial line", self.pending.len());
            self.pending.clear();
        }
    }
}

// --- Backoff ---

impl Backoff {
    /// A backoff starting at the floor.
    pub fn new() -> Self {
        Self { current: MIN_BACKOFF }
    }

    /// The delay to wait before the next attempt, doubling each time up to the ceiling.
    pub fn next_delay(&mut self) -> Duration {
        let delay = self.current;

        self.current = (self.current * 2).min(MAX_BACKOFF);

        delay
    }

    /// Returns to the floor after a successful connection.
    pub fn reset(&mut self) {
        self.current = MIN_BACKOFF;
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn splits_lines_across_reads() {
        let (mut a, b) = UnixStream::pair().unwrap();
        let mut reader = LineStream::from_stream(b, Duration::from_millis(50)).unwrap();
        let mut lines  = Vec::new();

        // A line arriving in two pieces must not be reported until it is complete.
        a.write_all(b"{\"seq\":1").unwrap();
        assert!(reader.poll(&mut lines).unwrap());
        assert!(lines.is_empty(), "a partial line must not be emitted");

        a.write_all(b"}\n{\"seq\":2}\n").unwrap();
        assert!(reader.poll(&mut lines).unwrap());
        assert_eq!(lines, vec!["{\"seq\":1}".to_string(), "{\"seq\":2}".to_string()]);
    }

    #[test]
    fn a_read_timeout_keeps_the_partial_line() {
        let (mut a, b) = UnixStream::pair().unwrap();
        let mut reader = LineStream::from_stream(b, Duration::from_millis(20)).unwrap();
        let mut lines  = Vec::new();

        a.write_all(b"half").unwrap();
        assert!(reader.poll(&mut lines).unwrap());

        // Poll again with nothing to read: this is the timeout path, and it must not
        // discard what was already buffered.
        assert!(reader.poll(&mut lines).unwrap());
        assert!(lines.is_empty());

        a.write_all(b"-a-line\n").unwrap();
        assert!(reader.poll(&mut lines).unwrap());
        assert_eq!(lines, vec!["half-a-line".to_string()]);
    }

    #[test]
    fn a_closed_peer_reports_end_of_stream() {
        let (a, b)     = UnixStream::pair().unwrap();
        let mut reader = LineStream::from_stream(b, Duration::from_millis(20)).unwrap();
        let mut lines  = Vec::new();

        drop(a);

        assert!(!reader.poll(&mut lines).unwrap(), "a dropped peer is end of stream");
        assert!(lines.is_empty());
    }

    #[test]
    fn carriage_returns_are_trimmed() {
        let (mut a, b) = UnixStream::pair().unwrap();
        let mut reader = LineStream::from_stream(b, Duration::from_millis(20)).unwrap();
        let mut lines  = Vec::new();

        a.write_all(b"one\r\ntwo\n").unwrap();
        reader.poll(&mut lines).unwrap();

        assert_eq!(lines, vec!["one".to_string(), "two".to_string()]);
    }

    #[test]
    fn backoff_doubles_up_to_the_ceiling_and_resets() {
        let mut b = Backoff::new();

        assert_eq!(b.next_delay(), MIN_BACKOFF);
        assert_eq!(b.next_delay(), MIN_BACKOFF * 2);
        assert_eq!(b.next_delay(), MIN_BACKOFF * 4);

        for _ in 0..20 {
            b.next_delay();
        }

        assert_eq!(b.next_delay(), MAX_BACKOFF, "the delay must not grow without bound");

        b.reset();
        assert_eq!(b.next_delay(), MIN_BACKOFF);
    }
}
