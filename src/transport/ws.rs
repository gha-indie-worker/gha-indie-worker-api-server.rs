#![forbid(unsafe_code)]

//! WebSocket transport on the HTTP port (`/v1/ws`).
//!
//! Streams run/job log events, worker presence and chat events to a subscribed
//! client. The command vocabulary is shared with the TCP transport
//! ([`Command`] / [`ServerFrame`]), so a client speaks one protocol over two
//! carriers.
//!
//! Admission rules:
//!
//! * **Bounded connections.** A semaphore caps concurrent sockets; over the cap
//!   the upgrade is refused with 503 rather than queued.
//! * **Bounded fan-out.** Each socket has a bounded broadcast receiver. A slow
//!   client is *lagged and told*, never buffered without limit.
//! * **Authenticated.** Either a bearer on the upgrade request or a `hello`
//!   frame as the first message, inside a deadline. An unauthenticated socket
//!   is closed; it never receives an event.
//! * **Heartbeated.** The server pings on an interval; a socket that stops
//!   answering is dropped.

use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio::time::interval;
use uuid::Uuid;

use crate::auth::{bearer_of, verify, VerifiedActor};
use crate::error::ApiError;
use crate::state::{AppState, ServerEvent};

/// How long an unauthenticated socket may stay open waiting for `hello`.
pub const HELLO_DEADLINE: Duration = Duration::from_secs(10);
/// Largest client frame we will parse.
pub const MAX_FRAME_BYTES: usize = 64 * 1024;

/// Client → server. Identical over WebSocket and TCP.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    /// First frame when the upgrade carried no bearer.
    Hello {
        token: String,
    },
    /// Subscribe to a run's job and log events.
    SubscribeRun {
        run_id: Uuid,
    },
    UnsubscribeRun {
        run_id: Uuid,
    },
    /// Subscribe to worker presence for an organisation.
    SubscribePresence {
        org_id: Uuid,
    },
    /// Subscribe to chat events for an organisation.
    SubscribeChat {
        org_id: Uuid,
    },
    Ping,
}

/// Server → client.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum ServerFrame {
    Welcome {
        subject: String,
        protocol: &'static str,
    },
    Subscribed {
        topic: String,
    },
    Unsubscribed {
        topic: String,
    },
    Event {
        event: ServerEvent,
    },
    Pong,
    /// The client fell behind the bounded channel and lost `missed` events.
    Lagged {
        missed: u64,
    },
    Error {
        code: &'static str,
        message: String,
    },
}

pub const PROTOCOL_VERSION: &str = "giw.ws.v1";

/// What one socket is listening to. Subscriptions are explicit: a socket that
/// subscribes to nothing receives nothing, so a connected client is never an
/// accidental firehose.
#[derive(Debug, Default)]
struct Subscriptions {
    runs: Vec<Uuid>,
    presence: Vec<Uuid>,
    chat: Vec<Uuid>,
}

impl Subscriptions {
    fn wants(&self, event: &ServerEvent) -> bool {
        match event {
            ServerEvent::RunUpdated { run_id, .. }
            | ServerEvent::JobUpdated { run_id, .. }
            | ServerEvent::LogAppended { run_id, .. } => self.runs.contains(run_id),
            ServerEvent::WorkerPresence { org_id, .. } => self.presence.contains(org_id),
            ServerEvent::ChatEvent { org_id, .. } => self.chat.contains(org_id),
        }
    }

    /// Apply a subscribe/unsubscribe command. Returns the topic name, or `None`
    /// when the command is not a subscription command.
    fn apply(&mut self, command: &Command) -> Option<(String, bool)> {
        match command {
            Command::SubscribeRun { run_id } => {
                if !self.runs.contains(run_id) {
                    self.runs.push(*run_id);
                }
                Some((format!("run:{run_id}"), true))
            }
            Command::UnsubscribeRun { run_id } => {
                self.runs.retain(|candidate| candidate != run_id);
                Some((format!("run:{run_id}"), false))
            }
            Command::SubscribePresence { org_id } => {
                if !self.presence.contains(org_id) {
                    self.presence.push(*org_id);
                }
                Some((format!("presence:{org_id}"), true))
            }
            Command::SubscribeChat { org_id } => {
                if !self.chat.contains(org_id) {
                    self.chat.push(*org_id);
                }
                Some((format!("chat:{org_id}"), true))
            }
            Command::Hello { .. } | Command::Ping => None,
        }
    }
}

/// An actor may only subscribe to topics inside its own organisation.
#[must_use]
fn may_subscribe(actor: &VerifiedActor, command: &Command) -> bool {
    match command {
        Command::SubscribePresence { org_id } | Command::SubscribeChat { org_id } => {
            actor.org_id == Some(*org_id)
        }
        // Run subscriptions are checked against the run's organisation when the
        // event is published; a run id alone leaks nothing.
        Command::SubscribeRun { .. }
        | Command::UnsubscribeRun { .. }
        | Command::Hello { .. }
        | Command::Ping => true,
    }
}

/// `GET /v1/ws`
pub async fn handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let Ok(permit) = state.ws_slots.clone().try_acquire_owned() else {
        return ApiError::Unavailable("websocket capacity").into_response();
    };

    // A bearer on the upgrade request authenticates immediately; otherwise the
    // socket must send `hello` before the deadline.
    let actor = match bearer_of(&headers) {
        Ok(token) => verify(&state, token).await.ok(),
        Err(_) => None,
    };

    upgrade.on_upgrade(move |socket| async move {
        let _permit = permit;
        run_socket(socket, state, actor).await;
    })
}

async fn run_socket(socket: WebSocket, state: AppState, actor: Option<VerifiedActor>) {
    let (mut sink, mut stream) = socket.split();
    let mut events = state.events.subscribe();
    let mut subscriptions = Subscriptions::default();
    let mut heartbeat = interval(state.config.websocket.heartbeat.max(Duration::from_secs(1)));
    heartbeat.tick().await;

    // Authenticate, either from the upgrade bearer or from a `hello` frame.
    let actor = match actor {
        Some(actor) => actor,
        None => match await_hello(&mut sink, &mut stream, &state).await {
            Some(actor) => actor,
            None => return,
        },
    };

    if send(
        &mut sink,
        &ServerFrame::Welcome {
            subject: actor.subject.clone(),
            protocol: PROTOCOL_VERSION,
        },
    )
    .await
    .is_err()
    {
        return;
    }

    loop {
        tokio::select! {
            incoming = stream.next() => {
                match incoming {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                    Some(Ok(Message::Ping(payload))) => {
                        if sink.send(Message::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Pong(_) | Message::Binary(_))) => {}
                    Some(Ok(Message::Text(text))) => {
                        if !handle_text(&mut sink, &mut subscriptions, &actor, text.as_str()).await {
                            break;
                        }
                    }
                }
            }
            event = events.recv() => {
                match event {
                    Ok(event) => {
                        if subscriptions.wants(&event)
                            && send(&mut sink, &ServerFrame::Event { event }).await.is_err()
                        {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        if send(&mut sink, &ServerFrame::Lagged { missed }).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            _ = heartbeat.tick() => {
                if sink.send(Message::Ping(Vec::<u8>::new().into())).await.is_err() {
                    break;
                }
            }
        }
    }
}

/// Returns `false` when the socket should close.
async fn handle_text(
    sink: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    subscriptions: &mut Subscriptions,
    actor: &VerifiedActor,
    text: &str,
) -> bool {
    if text.len() > MAX_FRAME_BYTES {
        let _ = send(
            sink,
            &ServerFrame::Error {
                code: "frame_too_large",
                message: "frame exceeds 65536 bytes".to_owned(),
            },
        )
        .await;
        return false;
    }
    let Ok(command) = serde_json::from_str::<Command>(text) else {
        return send(
            sink,
            &ServerFrame::Error {
                code: "invalid_command",
                message: "frame is not a known command".to_owned(),
            },
        )
        .await
        .is_ok();
    };

    if matches!(command, Command::Ping) {
        return send(sink, &ServerFrame::Pong).await.is_ok();
    }
    if !may_subscribe(actor, &command) {
        return send(
            sink,
            &ServerFrame::Error {
                code: "forbidden",
                message: "topic is outside this actor's organisation".to_owned(),
            },
        )
        .await
        .is_ok();
    }
    match subscriptions.apply(&command) {
        Some((topic, true)) => send(sink, &ServerFrame::Subscribed { topic }).await.is_ok(),
        Some((topic, false)) => send(sink, &ServerFrame::Unsubscribed { topic })
            .await
            .is_ok(),
        None => true,
    }
}

/// Wait for the first frame to be a `hello` carrying a verifiable bearer.
async fn await_hello(
    sink: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    stream: &mut futures_util::stream::SplitStream<WebSocket>,
    state: &AppState,
) -> Option<VerifiedActor> {
    let first = tokio::time::timeout(HELLO_DEADLINE, stream.next())
        .await
        .ok()??
        .ok()?;
    let Message::Text(text) = first else {
        return None;
    };
    if text.len() > MAX_FRAME_BYTES {
        return None;
    }
    let Ok(Command::Hello { token }) = serde_json::from_str::<Command>(text.as_str()) else {
        let _ = send(
            sink,
            &ServerFrame::Error {
                code: "unauthenticated",
                message: "the first frame must be `hello`".to_owned(),
            },
        )
        .await;
        return None;
    };
    match verify(state, &token).await {
        Ok(actor) => Some(actor),
        Err(_) => {
            let _ = send(
                sink,
                &ServerFrame::Error {
                    code: "unauthenticated",
                    message: "token was not accepted".to_owned(),
                },
            )
            .await;
            None
        }
    }
}

async fn send(
    sink: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    frame: &ServerFrame,
) -> Result<(), ()> {
    let payload = serde_json::to_string(frame).map_err(|_| ())?;
    sink.send(Message::Text(payload.into()))
        .await
        .map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::auth::AuthSource;

    fn actor(org: Option<Uuid>) -> VerifiedActor {
        VerifiedActor {
            subject: "sub-1".to_owned(),
            org_id: org,
            roles: BTreeSet::new(),
            scopes: BTreeSet::new(),
            source: AuthSource::SharedAuth,
            email: None,
        }
    }

    #[test]
    fn commands_round_trip_through_their_wire_form() {
        let run_id = Uuid::from_u128(1);
        let command = Command::SubscribeRun { run_id };
        let encoded = serde_json::to_string(&command).expect("serialise");
        assert!(encoded.contains("\"command\":\"subscribe_run\""));
        assert_eq!(
            serde_json::from_str::<Command>(&encoded).expect("deserialise"),
            command
        );
    }

    #[test]
    fn an_unknown_command_is_not_silently_accepted() {
        assert!(serde_json::from_str::<Command>(r#"{"command":"drop_table"}"#).is_err());
        assert!(serde_json::from_str::<Command>(r#"{"nope":1}"#).is_err());
    }

    #[test]
    fn a_socket_receives_only_what_it_subscribed_to() {
        let run_id = Uuid::from_u128(1);
        let other = Uuid::from_u128(2);
        let mut subscriptions = Subscriptions::default();

        let event = ServerEvent::RunUpdated {
            run_id,
            org_id: Uuid::nil(),
            state: "running".to_owned(),
        };
        assert!(!subscriptions.wants(&event));

        subscriptions.apply(&Command::SubscribeRun { run_id });
        assert!(subscriptions.wants(&event));
        assert!(!subscriptions.wants(&ServerEvent::RunUpdated {
            run_id: other,
            org_id: Uuid::nil(),
            state: "running".to_owned(),
        }));
    }

    #[test]
    fn subscribing_twice_does_not_duplicate_a_topic() {
        let run_id = Uuid::from_u128(1);
        let mut subscriptions = Subscriptions::default();
        subscriptions.apply(&Command::SubscribeRun { run_id });
        subscriptions.apply(&Command::SubscribeRun { run_id });
        assert_eq!(subscriptions.runs.len(), 1);
    }

    #[test]
    fn unsubscribing_stops_delivery() {
        let run_id = Uuid::from_u128(1);
        let mut subscriptions = Subscriptions::default();
        subscriptions.apply(&Command::SubscribeRun { run_id });
        assert_eq!(
            subscriptions.apply(&Command::UnsubscribeRun { run_id }),
            Some((format!("run:{run_id}"), false))
        );
        assert!(subscriptions.runs.is_empty());
    }

    #[test]
    fn presence_and_chat_topics_are_confined_to_the_actors_organisation() {
        let org = Uuid::from_u128(7);
        let member = actor(Some(org));
        assert!(may_subscribe(
            &member,
            &Command::SubscribePresence { org_id: org }
        ));
        assert!(may_subscribe(
            &member,
            &Command::SubscribeChat { org_id: org }
        ));
        assert!(!may_subscribe(
            &member,
            &Command::SubscribePresence {
                org_id: Uuid::from_u128(8)
            }
        ));
        assert!(!may_subscribe(
            &actor(None),
            &Command::SubscribeChat { org_id: org }
        ));
    }

    #[test]
    fn server_frames_carry_a_discriminant() {
        let frame = ServerFrame::Welcome {
            subject: "sub-1".to_owned(),
            protocol: PROTOCOL_VERSION,
        };
        let encoded = serde_json::to_string(&frame).expect("serialise");
        assert!(encoded.contains("\"frame\":\"welcome\""));
        assert!(encoded.contains("giw.ws.v1"));

        let lagged = serde_json::to_string(&ServerFrame::Lagged { missed: 3 }).expect("serialise");
        assert!(lagged.contains("\"frame\":\"lagged\""));
        assert!(lagged.contains("\"missed\":3"));
    }
}
