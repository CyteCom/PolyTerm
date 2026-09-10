//! Session logging to a file (FR-50).
//!
//! Logging taps the session's *output* — what the far end sent — and writes it
//! to a file with a timestamp at the start of every line. It is generic: it
//! sits at the UI's output tap, above the `Transport`, so it is written once
//! and serves SSH, serial, and local shells alike (ADR-5). It is toggled at
//! runtime; starting opens a file, stopping (dropping the handle) flushes and
//! closes it.
//!
//! File I/O never touches the UI thread: the handle hands bytes to a dedicated
//! writer thread over a bounded channel, and if that channel backs up under an
//! extreme burst, bytes are dropped rather than stalling the paint loop — a
//! lossy log beats a frozen window. A dedicated thread (rather than the tokio
//! blocking pool) is used so that dropping the handle can *join* the writer and
//! guarantee the file is flushed and closed.

use std::fs::OpenOptions;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::thread::JoinHandle;

use bytes::Bytes;

/// Bytes the log writer can buffer before the UI's non-blocking sends start
/// dropping. Generous, since a serial or shell stream is modest.
const LOG_CHANNEL_CAPACITY: usize = 4096;

/// A running session log. Drop it to stop logging: the writer thread is joined,
/// so the file is guaranteed flushed and closed.
#[derive(Debug)]
pub struct SessionLog {
    /// `Option` so `Drop` can close the channel before joining the writer.
    tx: Option<SyncSender<Bytes>>,
    writer: Option<JoinHandle<()>>,
    path: PathBuf,
}

impl SessionLog {
    /// Start logging session output to `path`, appending if it exists. Opens
    /// the file up front so a bad path is reported now, then writes on a
    /// dedicated thread.
    pub fn start(path: PathBuf) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let (tx, rx) = sync_channel::<Bytes>(LOG_CHANNEL_CAPACITY);
        let writer = std::thread::Builder::new()
            .name("polyterm-log".to_owned())
            .spawn(move || {
                let mut writer = LineTimestamper::new(BufWriter::new(file), timestamp);
                while let Ok(chunk) = rx.recv() {
                    // A write error (disk full, unplugged medium) ends logging
                    // quietly; it must not take down the session.
                    if writer.write_bytes(&chunk).is_err() {
                        break;
                    }
                    // Flush each chunk so a log being watched live (a boot log)
                    // is current. Serial and shell rates make this cheap.
                    let _ = writer.flush();
                }
                let _ = writer.flush();
            })?;
        Ok(Self {
            tx: Some(tx),
            writer: Some(writer),
            path,
        })
    }

    /// Hand output bytes to the log. Non-blocking; drops on a full buffer.
    pub fn write(&self, bytes: &[u8]) {
        if let Some(tx) = &self.tx {
            let _ = tx.try_send(Bytes::copy_from_slice(bytes));
        }
    }

    /// The file being written.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for SessionLog {
    fn drop(&mut self) {
        // Close the channel so the writer loop ends, then join it so the file
        // is flushed and closed before we return.
        self.tx.take();
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

/// A default log path: `polyterm-<timestamp>.log` under `POLYTERM_LOG_DIR`, or
/// the system temp directory. Used by the runtime toggle, which has no file
/// dialog yet (that is UI chrome for M4).
pub fn default_log_path() -> PathBuf {
    let dir = std::env::var_os("POLYTERM_LOG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let z = jiff::Zoned::now();
    let name = format!(
        "polyterm-{:04}{:02}{:02}-{:02}{:02}{:02}.log",
        z.year(),
        z.month(),
        z.day(),
        z.hour(),
        z.minute(),
        z.second(),
    );
    dir.join(name)
}

/// The current local time as `[YYYY-MM-DD HH:MM:SS.mmm] `.
fn timestamp() -> String {
    let z = jiff::Zoned::now();
    format!(
        "[{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}] ",
        z.year(),
        z.month(),
        z.day(),
        z.hour(),
        z.minute(),
        z.second(),
        z.millisecond(),
    )
}

/// Wraps a writer and inserts a timestamp at the start of every line.
///
/// The clock is injected so the line logic can be tested without the wall
/// clock. A byte-oriented stream has no notion of "line" beyond `\n`, so that
/// is the boundary: after each newline, the next byte begins a timestamped line.
struct LineTimestamper<W: Write, C: FnMut() -> String> {
    sink: W,
    clock: C,
    at_line_start: bool,
}

impl<W: Write, C: FnMut() -> String> LineTimestamper<W, C> {
    fn new(sink: W, clock: C) -> Self {
        Self {
            sink,
            clock,
            at_line_start: true,
        }
    }

    fn write_bytes(&mut self, bytes: &[u8]) -> io::Result<()> {
        for &byte in bytes {
            if self.at_line_start {
                self.sink.write_all((self.clock)().as_bytes())?;
                self.at_line_start = false;
            }
            self.sink.write_all(&[byte])?;
            if byte == b'\n' {
                self.at_line_start = true;
            }
        }
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.sink.flush()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A timestamper with a fixed clock over an in-memory sink.
    fn fixed() -> LineTimestamper<Vec<u8>, impl FnMut() -> String> {
        LineTimestamper::new(Vec::new(), || "T ".to_owned())
    }

    #[test]
    fn each_line_is_prefixed_once() {
        let mut ts = fixed();
        ts.write_bytes(b"hello\nworld\n").unwrap();
        assert_eq!(ts.sink, b"T hello\nT world\n");
    }

    #[test]
    fn a_prefix_is_added_only_at_a_real_line_start() {
        let mut ts = fixed();
        // Split a single line across two writes: the prefix goes on once, at
        // the start, not again mid-line.
        ts.write_bytes(b"abc").unwrap();
        ts.write_bytes(b"def\n").unwrap();
        assert_eq!(ts.sink, b"T abcdef\n");
    }

    #[test]
    fn a_trailing_newline_defers_the_next_prefix() {
        let mut ts = fixed();
        ts.write_bytes(b"one\n").unwrap();
        ts.write_bytes(b"two").unwrap();
        assert_eq!(ts.sink, b"T one\nT two");
    }

    #[test]
    fn real_timestamp_has_the_expected_shape() {
        // [YYYY-MM-DD HH:MM:SS.mmm] with a trailing space: 26 characters.
        let stamp = timestamp();
        assert_eq!(stamp.len(), 26, "{stamp:?}");
        assert!(stamp.starts_with('['));
        assert!(stamp.ends_with("] "));
    }

    #[test]
    fn end_to_end_writes_timestamped_lines_to_the_file() {
        let path = std::env::temp_dir().join(format!(
            "polyterm-logtest-{}-{:?}.log",
            std::process::id(),
            std::thread::current().id(),
        ));
        let _ = std::fs::remove_file(&path);

        // Drop joins the writer, so the file is complete after this scope.
        {
            let log = SessionLog::start(path.clone()).unwrap();
            assert_eq!(log.path(), path);
            log.write(b"alpha\n");
            log.write(b"beta\n");
        }

        let content = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "content was {content:?}");
        // Each line is "[timestamp] text": bracketed prefix, then the payload.
        assert!(lines[0].starts_with('['));
        assert!(lines[0].ends_with("alpha"));
        assert!(lines[1].ends_with("beta"));
        assert!(lines[0].len() > 26);
    }
}
