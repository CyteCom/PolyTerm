//! Local shell sessions over the platform PTY.
//!
//! Implements [`Transport`] over `portable-pty` — ConPTY on Windows, forkpty on
//! Linux (FR-55, FR-56). `portable-pty` is a blocking API, so this crate is the
//! textbook case for the threading rule in `ARCHITECTURE.md` §1: the PTY read
//! and write loops run on the tokio blocking pool via `spawn_blocking`, never on
//! a runtime worker and never on the UI thread. A small async task owns the
//! master handle for resize and waits for the child to exit.
//!
//! The only fallible, synchronous work — opening the PTY and spawning the shell
//! — happens in [`PtyTransport::spawn`] itself so that a failure to launch is
//! returned to the caller directly rather than surfaced later as an event.

#![forbid(unsafe_code)]

use std::io::{Read, Write};

use bytes::Bytes;
use polyterm_core::{
    ControlMsg, DisconnectReason, PtyConfig, Transport, TransportBackendEnd, TransportError,
    TransportEvent, TransportHandle, TransportKind,
};
use portable_pty::{Child, ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::runtime::Handle;

/// A local shell backend. One value opens every local-shell session for the
/// life of the process; constructing it does no I/O.
#[derive(Debug, Default)]
pub struct PtyTransport;

/// The PTY size used until the UI sends its first [`ControlMsg::Resize`]. A
/// terminal always resizes to its pane on the first frame, so this is a
/// placeholder, not a policy.
const INITIAL_SIZE: PtySize = PtySize {
    rows: 24,
    cols: 80,
    pixel_width: 0,
    pixel_height: 0,
};

/// Chunk size for PTY reads. Large enough that a fast producer (`cat`) is not
/// syscall-bound, small enough to keep latency low on interactive output.
const READ_CHUNK: usize = 4096;

impl Transport for PtyTransport {
    type Config = PtyConfig;

    const KIND: TransportKind = TransportKind::LocalShell;

    fn spawn(&self, rt: &Handle, cfg: PtyConfig) -> Result<TransportHandle, TransportError> {
        // Synchronous, fallible setup first, so launch failures are returned
        // rather than posted as an event after the fact.
        let pty = native_pty_system()
            .openpty(INITIAL_SIZE)
            .map_err(|e| backend_err("open pty", &e))?;

        let command = build_command(cfg);
        let child = pty
            .slave
            .spawn_command(command)
            .map_err(|e| backend_err("spawn shell", &e))?;

        // Dropping the slave in the parent lets the master reader see EOF when
        // the child exits; holding it open would wedge the read loop forever.
        drop(pty.slave);

        let reader = pty
            .master
            .try_clone_reader()
            .map_err(|e| backend_err("clone pty reader", &e))?;
        let writer = pty
            .master
            .take_writer()
            .map_err(|e| backend_err("take pty writer", &e))?;
        let master = pty.master;

        // A killer, taken before the child is moved into the wait task, lets the
        // control task terminate the child from a different thread.
        let killer = child.clone_killer();

        let (ui, backend) = TransportHandle::new_pair();
        let TransportBackendEnd {
            output,
            input,
            control,
            events,
        } = backend;

        spawn_reader(rt, reader, output);
        spawn_writer(rt, writer, input);
        spawn_controller(rt, master, killer, child, control, events);

        Ok(ui)
    }
}

fn build_command(cfg: PtyConfig) -> CommandBuilder {
    let mut command = match cfg.shell {
        Some(shell) => CommandBuilder::new(shell),
        None => CommandBuilder::new_default_prog(),
    };
    if let Some(cwd) = cfg.working_directory {
        command.cwd(cwd);
    }
    for (key, value) in cfg.env {
        command.env(key, value);
    }
    command
}

/// Reader loop: blocking PTY reads → the bounded `output` channel. When the
/// channel is full the read blocks, which is the backpressure NFR-5 relies on.
fn spawn_reader(
    rt: &Handle,
    mut reader: Box<dyn Read + Send>,
    output: tokio::sync::mpsc::Sender<Bytes>,
) {
    rt.spawn_blocking(move || {
        let mut buf = [0u8; READ_CHUNK];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break, // EOF: the child closed the PTY.
                Ok(n) => {
                    if output
                        .blocking_send(Bytes::copy_from_slice(&buf[..n]))
                        .is_err()
                    {
                        break; // The UI dropped the receiver.
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });
}

/// Writer loop: the `input` channel → blocking PTY writes.
fn spawn_writer(
    rt: &Handle,
    mut writer: Box<dyn Write + Send>,
    mut input: tokio::sync::mpsc::Receiver<Bytes>,
) {
    rt.spawn_blocking(move || {
        while let Some(bytes) = input.blocking_recv() {
            if writer.write_all(&bytes).is_err() {
                break;
            }
            let _ = writer.flush();
        }
    });
}

/// Control and lifecycle: owns the master (for resize), applies control
/// messages, and reports the single `Disconnected` event when the child exits
/// — whether it exited on its own or was killed on request.
fn spawn_controller(
    rt: &Handle,
    master: Box<dyn MasterPty + Send>,
    mut killer: Box<dyn ChildKiller + Send + Sync>,
    child: Box<dyn Child + Send + Sync>,
    mut control: tokio::sync::mpsc::Receiver<ControlMsg>,
    events: tokio::sync::mpsc::Sender<TransportEvent>,
) {
    let wait_rt = rt.clone();
    rt.spawn(async move {
        // A local PTY is usable the moment it is spawned.
        let _ = events.send(TransportEvent::Connected).await;

        let mut wait = wait_rt.spawn_blocking(move || child_wait(child));

        let reason = loop {
            tokio::select! {
                msg = control.recv() => match msg {
                    Some(ControlMsg::Resize { cols, rows }) => {
                        let _ = master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
                    }
                    // An explicit Disconnect, or the UI dropping the control
                    // channel: kill the child and wait for it to be reaped below.
                    Some(ControlMsg::Disconnect) | None => {
                        let _ = killer.kill();
                        break DisconnectReason::Local;
                    }
                    // Break, DTR/RTS, and Reconnect have no meaning for a local
                    // PTY. Ignoring them is the design (ADR-5), not a gap.
                    Some(_) => {}
                },
                _ = &mut wait => {
                    // The shell exited on its own.
                    break DisconnectReason::Remote;
                }
            }
        };

        if !wait.is_finished() {
            let _ = wait.await;
        }
        let _ = events.send(TransportEvent::Disconnected { reason }).await;
    });
}

/// Blocking wait for the child, on the blocking pool. The exit status is not
/// surfaced beyond "the session ended"; a local shell exiting is normal.
fn child_wait(mut child: Box<dyn Child + Send + Sync>) {
    let _ = child.wait();
}

/// Map a `portable-pty` error (an `anyhow::Error`) into a transport error
/// without taking an `anyhow` dependency in this library crate (CLAUDE.md §5).
/// The message is preserved; the source chain is not.
fn backend_err(context: &str, err: &dyn std::fmt::Display) -> TransportError {
    TransportError::Backend {
        kind: TransportKind::LocalShell,
        source: format!("{context}: {err}").into(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::runtime::Runtime;
    use tokio::time::timeout;

    const DEADLINE: Duration = Duration::from_secs(20);

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn kind_is_local_shell() {
        assert_eq!(PtyTransport::KIND, TransportKind::LocalShell);
    }

    #[test]
    fn shell_round_trips_input_to_output_and_exits() {
        let rt = Runtime::new().unwrap();
        let handle = PtyTransport
            .spawn(rt.handle(), PtyConfig::default())
            .expect("spawn a local shell");

        rt.block_on(async move {
            // Own each channel independently so output-read and input-write can
            // interleave without borrow conflicts.
            let TransportHandle {
                mut output,
                input,
                control,
                mut events,
            } = handle;

            // Wait for Connected before driving the session.
            let connected = timeout(DEADLINE, async {
                while let Some(ev) = events.recv().await {
                    if matches!(ev, TransportEvent::Connected) {
                        return true;
                    }
                }
                false
            })
            .await
            .unwrap_or(false);
            assert!(connected, "no Connected event");

            control
                .send(ControlMsg::Resize { cols: 80, rows: 24 })
                .await
                .unwrap();

            // Both cmd.exe and sh understand `echo <x>` terminated by CRLF.
            input
                .send(Bytes::from_static(b"echo polyterm_marker\r\n"))
                .await
                .unwrap();

            // This crate is a dumb byte pipe; answering device queries is the
            // terminal emulator's job (polyterm-term). But ConPTY's cmd.exe
            // blocks at startup on a cursor-position report (ESC[6n) until the
            // "terminal" replies, so the harness stands in for one here — a
            // minimal CPR — or nothing would ever run. On Unix `sh` sends no
            // such query and this branch simply never fires.
            let mut acc = Vec::new();
            let mut answered_dsr = false;
            let saw_marker = timeout(DEADLINE, async {
                while let Some(chunk) = output.recv().await {
                    acc.extend_from_slice(&chunk);
                    if !answered_dsr && contains(&acc, b"\x1b[6n") {
                        answered_dsr = true;
                        let _ = input.send(Bytes::from_static(b"\x1b[1;1R")).await;
                    }
                    if contains(&acc, b"polyterm_marker") {
                        return true;
                    }
                }
                false
            })
            .await
            .unwrap_or(false);
            assert!(
                saw_marker,
                "marker not seen in output: {}",
                String::from_utf8_lossy(&acc)
            );

            // Exiting the shell must surface as Disconnected, not a silent stop.
            input.send(Bytes::from_static(b"exit\r\n")).await.unwrap();
            let disconnected = timeout(DEADLINE, async {
                while let Some(ev) = events.recv().await {
                    if matches!(ev, TransportEvent::Disconnected { .. }) {
                        return true;
                    }
                }
                false
            })
            .await
            .unwrap_or(false);
            assert!(disconnected, "shell exit did not produce Disconnected");
        });
    }

    #[test]
    fn disconnect_control_terminates_the_session() {
        let rt = Runtime::new().unwrap();
        let mut handle = PtyTransport
            .spawn(rt.handle(), PtyConfig::default())
            .expect("spawn a local shell");

        rt.block_on(async {
            handle.control.send(ControlMsg::Disconnect).await.unwrap();
            let disconnected = timeout(DEADLINE, async {
                while let Some(ev) = handle.events.recv().await {
                    if let TransportEvent::Disconnected { reason } = ev {
                        return Some(reason);
                    }
                }
                None
            })
            .await
            .unwrap_or(None);
            assert_eq!(disconnected, Some(DisconnectReason::Local));
        });
    }
}
