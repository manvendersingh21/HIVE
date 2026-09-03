//! WebSocket ↔ PTY ↔ `tmux attach` bridge.
//!
//! One browser tab gets one PTY running `tmux attach-session`. Because the
//! attach is a real tmux client, closing the tab detaches rather than kills —
//! the session keeps running, which is the whole reason work goes through tmux
//! in the first place.
//!
//! Wire protocol, both directions over one socket:
//!   * client → server: binary frames are raw keystrokes; text frames are
//!     JSON control messages (currently only `resize`).
//!   * server → client: binary frames are raw terminal output.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use axum::extract::ws::{Message, WebSocket};
use futures::{SinkExt, StreamExt};
use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// Control messages a browser can send.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Control {
    Resize { cols: u16, rows: u16 },
}

/// Bytes read from the PTY in one go. tmux redraws can be large.
const READ_BUF: usize = 8192;

pub async fn bridge(socket: WebSocket, session: String, cols: u16, rows: u16) {
    if let Err(e) = run(socket, &session, cols, rows).await {
        warn!(session = %session, error = %e, "terminal bridge ended with error");
    } else {
        debug!(session = %session, "terminal bridge closed cleanly");
    }
}

async fn run(socket: WebSocket, session: &str, cols: u16, rows: u16) -> anyhow::Result<()> {
    let pty = NativePtySystem::default();
    let pair = pty.openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let mut cmd = CommandBuilder::new("tmux");
    // `-t =name` pins the exact session; without `=` tmux prefix-matches and a
    // request for "build" could attach to "build-arm64".
    cmd.args(["attach-session", "-t", &format!("={session}")]);
    cmd.env("TERM", "xterm-256color");
    // A detach here means the browser tab closed, not that the session died.
    let mut child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader()?;
    let mut writer = pair.master.take_writer()?;
    let master = Arc::new(Mutex::new(pair.master));

    let (ws_tx, mut ws_rx) = socket.split();
    let ws_tx = Arc::new(tokio::sync::Mutex::new(ws_tx));

    // PTY reads are blocking, so they get a dedicated OS thread that hands
    // bytes to async via a channel.
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(64);
    let reader_thread = std::thread::spawn(move || {
        let mut buf = [0u8; READ_BUF];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if out_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Same story for writes.
    let (in_tx, in_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let writer_thread = std::thread::spawn(move || {
        while let Ok(bytes) = in_rx.recv() {
            if writer.write_all(&bytes).is_err() || writer.flush().is_err() {
                break;
            }
        }
    });

    // Pump PTY output to the browser.
    let pump_out = {
        let ws_tx = Arc::clone(&ws_tx);
        tokio::spawn(async move {
            while let Some(chunk) = out_rx.recv().await {
                if ws_tx
                    .lock()
                    .await
                    .send(Message::Binary(chunk.into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        })
    };

    // Pump browser input to the PTY.
    while let Some(Ok(msg)) = ws_rx.next().await {
        match msg {
            Message::Binary(data) => {
                if in_tx.send(data.to_vec()).is_err() {
                    break;
                }
            }
            Message::Text(text) => match serde_json::from_str::<Control>(&text) {
                Ok(Control::Resize { cols, rows }) => {
                    if let Ok(m) = master.lock() {
                        let _ = m.resize(PtySize {
                            rows,
                            cols,
                            pixel_width: 0,
                            pixel_height: 0,
                        });
                    }
                }
                Err(e) => debug!(error = %e, "ignoring unrecognized control frame"),
            },
            Message::Close(_) => break,
            _ => {}
        }
    }

    // Browser is gone: detach the tmux client and reap everything.
    let _ = child.kill();
    let _ = child.wait();
    drop(in_tx);
    pump_out.abort();
    let _ = writer_thread.join();
    let _ = reader_thread.join();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_resize_control_frame() {
        let Control::Resize { cols, rows } =
            serde_json::from_str(r#"{"type":"resize","cols":120,"rows":40}"#).expect("parses");
        assert_eq!((cols, rows), (120, 40));
    }

    #[test]
    fn rejects_unknown_control_frames() {
        assert!(serde_json::from_str::<Control>(r#"{"type":"exec","cmd":"rm -rf /"}"#).is_err());
    }
}
