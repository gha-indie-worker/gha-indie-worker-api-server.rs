#![forbid(unsafe_code)]

//! Stateful TCP transport (`GHA_INDIE_WORKER_API_TCP_BIND`).
//!
//! Frames are length-prefixed JSON: a big-endian `u32` byte count followed by
//! exactly that many bytes of UTF-8 JSON. The command vocabulary is the *same*
//! [`Command`] / [`ServerFrame`] pair the WebSocket transport speaks, so a
//! client picks a carrier, not a protocol.
//!
//! Length-prefixing is what makes the reader safe: the frame length is checked
//! against [`MAX_FRAME_BYTES`] *before* a buffer is allocated, so a hostile
//! peer cannot ask the process to allocate four gigabytes.

use std::io;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;

use crate::auth::{verify, VerifiedActor};
use crate::state::{AppState, ServerEvent};
use crate::transport::ws::{Command, ServerFrame, MAX_FRAME_BYTES, PROTOCOL_VERSION};

/// How long a connection may stay unauthenticated.
pub const HELLO_DEADLINE: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame length {0} exceeds the 65536 byte bound")]
    TooLarge(u32),
    #[error("frame is not valid UTF-8 JSON")]
    Malformed,
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Encode one frame. The length prefix always matches the payload length.
///
/// # Errors
/// Returns [`FrameError::TooLarge`] when the encoded payload exceeds the bound
/// and [`FrameError::Malformed`] when the value cannot be serialised.
pub fn encode(frame: &ServerFrame) -> Result<Vec<u8>, FrameError> {
    let payload = serde_json::to_vec(frame).map_err(|_| FrameError::Malformed)?;
    let length = u32::try_from(payload.len()).map_err(|_| FrameError::TooLarge(u32::MAX))?;
    if payload.len() > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(length));
    }
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Decode a payload that a caller has already length-checked.
///
/// # Errors
/// Returns [`FrameError::Malformed`] when the bytes are not a known command.
pub fn decode(payload: &[u8]) -> Result<Command, FrameError> {
    serde_json::from_slice(payload).map_err(|_| FrameError::Malformed)
}

/// Check a length prefix before any allocation happens.
///
/// # Errors
/// Returns [`FrameError::TooLarge`] when the declared length is above the bound.
pub fn check_length(length: u32) -> Result<usize, FrameError> {
    if length as usize > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(length));
    }
    Ok(length as usize)
}

/// Bind and serve until `shutdown` resolves.
///
/// # Errors
/// Returns the bind error when the listener cannot be created. Per-connection
/// failures are logged and never take the listener down.
pub async fn serve(
    bind: &str,
    state: AppState,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> io::Result<()> {
    let listener = TcpListener::bind(bind).await?;
    let local = listener.local_addr()?;
    tracing::info!(bind = %local, protocol = PROTOCOL_VERSION, "tcp transport listening");

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, peer)) => {
                        let state = state.clone();
                        tokio::spawn(async move {
                            if let Err(error) = handle(stream, state).await {
                                tracing::debug!(%peer, %error, "tcp connection ended");
                            }
                        });
                    }
                    Err(error) => {
                        tracing::warn!(%error, "tcp accept failed");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    tracing::info!("tcp transport draining");
                    return Ok(());
                }
            }
        }
    }
}

async fn handle(mut stream: TcpStream, state: AppState) -> Result<(), FrameError> {
    // Reading and writing are separate borrows, so one connection can serve a
    // client command and a broadcast event in the same `select!`.
    let (mut reader, mut writer) = stream.split();

    let actor = tokio::time::timeout(HELLO_DEADLINE, authenticate(&mut reader, &state))
        .await
        .map_err(|_| FrameError::Malformed)??;

    write_frame(
        &mut writer,
        &ServerFrame::Welcome {
            subject: actor.subject.clone(),
            protocol: PROTOCOL_VERSION,
        },
    )
    .await?;

    let mut events = state.events.subscribe();
    let mut subscribed_runs: Vec<uuid::Uuid> = Vec::new();

    loop {
        tokio::select! {
            frame = read_frame(&mut reader) => {
                let Some(payload) = frame? else { return Ok(()) };
                let command = decode(&payload)?;
                let reply = apply(&command, &mut subscribed_runs);
                write_frame(&mut writer, &reply).await?;
            }
            event = events.recv() => {
                match event {
                    Ok(event) => {
                        if wants(&subscribed_runs, &event) {
                            write_frame(&mut writer, &ServerFrame::Event { event }).await?;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        write_frame(&mut writer, &ServerFrame::Lagged { missed }).await?;
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }
        }
    }
}

async fn authenticate<R: AsyncRead + Unpin>(
    reader: &mut R,
    state: &AppState,
) -> Result<VerifiedActor, FrameError> {
    let Some(payload) = read_frame(reader).await? else {
        return Err(FrameError::Malformed);
    };
    let Command::Hello { token } = decode(&payload)? else {
        return Err(FrameError::Malformed);
    };
    verify(state, &token)
        .await
        .map_err(|_| FrameError::Malformed)
}

/// Apply a command to this connection's subscriptions and produce the reply.
#[must_use]
pub fn apply(command: &Command, runs: &mut Vec<uuid::Uuid>) -> ServerFrame {
    match command {
        Command::Ping => ServerFrame::Pong,
        Command::SubscribeRun { run_id } => {
            if !runs.contains(run_id) {
                runs.push(*run_id);
            }
            ServerFrame::Subscribed {
                topic: format!("run:{run_id}"),
            }
        }
        Command::UnsubscribeRun { run_id } => {
            runs.retain(|candidate| candidate != run_id);
            ServerFrame::Unsubscribed {
                topic: format!("run:{run_id}"),
            }
        }
        Command::Hello { .. } => ServerFrame::Error {
            code: "already_authenticated",
            message: "hello may only be the first frame".to_owned(),
        },
        Command::SubscribePresence { .. } | Command::SubscribeChat { .. } => ServerFrame::Error {
            code: "unsupported_topic",
            message: "presence and chat are WebSocket-only topics".to_owned(),
        },
    }
}

#[must_use]
pub fn wants(runs: &[uuid::Uuid], event: &ServerEvent) -> bool {
    match event {
        ServerEvent::RunUpdated { run_id, .. }
        | ServerEvent::JobUpdated { run_id, .. }
        | ServerEvent::LogAppended { run_id, .. } => runs.contains(run_id),
        ServerEvent::WorkerPresence { .. } | ServerEvent::ChatEvent { .. } => false,
    }
}

/// Read one frame. `Ok(None)` means the peer closed cleanly.
async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Option<Vec<u8>>, FrameError> {
    let mut length = [0_u8; 4];
    match reader.read_exact(&mut length).await {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(FrameError::Io(error)),
    }
    // The bound is checked before the buffer is allocated.
    let size = check_length(u32::from_be_bytes(length))?;
    let mut payload = vec![0_u8; size];
    reader.read_exact(&mut payload).await?;
    Ok(Some(payload))
}

async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    frame: &ServerFrame,
) -> Result<(), FrameError> {
    let bytes = encode(frame)?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

/// Spawn the listener when a bind address is configured.
pub fn spawn(
    state: AppState,
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> Option<tokio::task::JoinHandle<()>> {
    let bind = state.config.tcp_bind.clone()?;
    Some(tokio::spawn(async move {
        if let Err(error) = serve(&bind, state, shutdown).await {
            tracing::error!(%error, "tcp transport failed to bind");
        }
    }))
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

    #[test]
    fn a_frame_round_trips_with_a_matching_length_prefix() {
        let frame = ServerFrame::Pong;
        let encoded = encode(&frame).expect("encodes");
        let length = u32::from_be_bytes([encoded[0], encoded[1], encoded[2], encoded[3]]);
        assert_eq!(length as usize, encoded.len() - 4);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&encoded[4..]).expect("json"),
            serde_json::json!({ "frame": "pong" })
        );
    }

    #[test]
    fn a_declared_length_is_bounded_before_anything_is_allocated() {
        assert_eq!(check_length(16).expect("within bound"), 16);
        assert!(check_length(MAX_FRAME_BYTES as u32).is_ok());
        assert!(matches!(
            check_length(MAX_FRAME_BYTES as u32 + 1),
            Err(FrameError::TooLarge(_))
        ));
        assert!(matches!(
            check_length(u32::MAX),
            Err(FrameError::TooLarge(_))
        ));
    }

    #[test]
    fn decoding_refuses_anything_that_is_not_a_known_command() {
        assert!(matches!(decode(b"not json"), Err(FrameError::Malformed)));
        assert!(matches!(
            decode(br#"{"command":"drop_table"}"#),
            Err(FrameError::Malformed)
        ));
        assert_eq!(
            decode(br#"{"command":"ping"}"#).expect("known command"),
            Command::Ping
        );
    }

    #[test]
    fn commands_are_applied_to_this_connections_subscriptions() {
        let run_id = Uuid::from_u128(1);
        let mut runs = Vec::new();

        assert_eq!(apply(&Command::Ping, &mut runs), ServerFrame::Pong);
        assert_eq!(
            apply(&Command::SubscribeRun { run_id }, &mut runs),
            ServerFrame::Subscribed {
                topic: format!("run:{run_id}")
            }
        );
        // Idempotent.
        let _ = apply(&Command::SubscribeRun { run_id }, &mut runs);
        assert_eq!(runs.len(), 1);

        let _ = apply(&Command::UnsubscribeRun { run_id }, &mut runs);
        assert!(runs.is_empty());
    }

    #[test]
    fn a_second_hello_is_an_error_not_a_re_authentication() {
        let mut runs = Vec::new();
        assert!(matches!(
            apply(
                &Command::Hello {
                    token: "x".to_owned()
                },
                &mut runs
            ),
            ServerFrame::Error {
                code: "already_authenticated",
                ..
            }
        ));
    }

    #[test]
    fn websocket_only_topics_are_refused_rather_than_silently_dropped() {
        let mut runs = Vec::new();
        assert!(matches!(
            apply(
                &Command::SubscribePresence {
                    org_id: Uuid::nil()
                },
                &mut runs
            ),
            ServerFrame::Error {
                code: "unsupported_topic",
                ..
            }
        ));
    }

    #[test]
    fn only_subscribed_runs_are_delivered_over_tcp() {
        let run_id = Uuid::from_u128(1);
        let event = ServerEvent::RunUpdated {
            run_id,
            org_id: Uuid::nil(),
            state: "running".to_owned(),
        };
        assert!(!wants(&[], &event));
        assert!(wants(&[run_id], &event));
        assert!(!wants(&[Uuid::from_u128(2)], &event));
        assert!(!wants(
            &[run_id],
            &ServerEvent::WorkerPresence {
                org_id: Uuid::nil(),
                worker_id: Uuid::nil(),
                state: "idle".to_owned(),
            }
        ));
    }
}
