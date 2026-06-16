//! Live message echo for the Measure panel.
//!
//! A single background task owns a dynamic subscriber for whichever topic is
//! currently selected in the Measure panel and decodes its latest message into
//! [`EchoState`], much like `ros2 topic echo <topic>`. Schema discovery and CDR
//! deserialization happen off the render loop so the UI never blocks.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::watch;

use hiroz::dynamic::DynSub;

use super::engine::CoreEngine;
use super::message_formatter::format_message_pretty;

/// How long to wait for schema discovery before giving up on a topic.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(3);
/// How often the worker drains the subscriber queue and refreshes the display.
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Maximum number of scrollback lines kept per echoed topic.
pub const MAX_ECHO_LINES: usize = 5000;

/// Status of the echo subscription for the currently selected topic.
#[derive(Default, Clone, PartialEq)]
pub enum EchoStatus {
    /// No topic is selected (e.g. the Measure list is empty).
    #[default]
    Idle,
    /// Creating the subscriber / waiting for the type description.
    Discovering,
    /// Subscriber is live but no message has arrived yet.
    NoData,
    /// Receiving and decoding messages.
    Active,
    /// Schema discovery or decoding failed.
    Error(String),
}

/// Shared, render-loop-readable snapshot of the echoed message stream.
#[derive(Default)]
pub struct EchoState {
    /// Topic the buffered `lines` belong to.
    pub topic: Option<String>,
    /// Resolved message type name, once schema discovery succeeds.
    pub type_name: Option<String>,
    /// Rolling scrollback of pretty-printed messages, capped at [`MAX_ECHO_LINES`].
    pub lines: VecDeque<String>,
    pub status: EchoStatus,
    /// Number of messages decoded since this topic was selected.
    pub msg_count: u64,
    pub last_update: Option<Instant>,
}

impl EchoState {
    fn reset_for(&mut self, topic: Option<String>, status: EchoStatus) {
        self.topic = topic;
        self.type_name = None;
        self.lines.clear();
        self.status = status;
        self.msg_count = 0;
        self.last_update = None;
    }

    /// Append a line, evicting the oldest once the cap is reached.
    fn push_line(&mut self, line: String) {
        if self.lines.len() >= MAX_ECHO_LINES {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }
}

/// Background task: follow the selected topic and keep [`EchoState`] up to date.
///
/// Returns when the watch sender is dropped (i.e. the app is shutting down).
pub async fn run_echo_worker(
    core: Arc<CoreEngine>,
    state: Arc<Mutex<EchoState>>,
    mut topic_rx: watch::Receiver<Option<String>>,
) {
    // The topic the active subscriber was created for, plus the subscriber.
    let mut current: Option<(String, DynSub)> = None;

    loop {
        tokio::select! {
            // The selected topic changed (or the app is shutting down).
            changed = topic_rx.changed() => {
                if changed.is_err() {
                    return; // sender dropped
                }
                let desired = topic_rx.borrow_and_update().clone();
                match desired {
                    None => {
                        current = None;
                        state.lock().reset_for(None, EchoStatus::Idle);
                    }
                    Some(topic) => {
                        // Already subscribed to this topic — nothing to do.
                        if current.as_ref().is_some_and(|(t, _)| *t == topic) {
                            continue;
                        }
                        current = None;
                        state.lock().reset_for(Some(topic.clone()), EchoStatus::Discovering);

                        match core.create_echo_subscriber(&topic, DISCOVERY_TIMEOUT).await {
                            Ok(sub) => {
                                let type_name = sub.schema().map(|s| s.type_name.clone());
                                // Apply only if the selection hasn't moved on while we waited.
                                let mut s = state.lock();
                                if s.topic.as_deref() == Some(topic.as_str()) {
                                    s.type_name = type_name;
                                    s.status = EchoStatus::NoData;
                                }
                                drop(s);
                                current = Some((topic, sub));
                            }
                            Err(e) => {
                                let mut s = state.lock();
                                if s.topic.as_deref() == Some(topic.as_str()) {
                                    s.status = EchoStatus::Error(e.to_string());
                                }
                            }
                        }
                    }
                }
            }

            // Periodically drain the subscriber and show the most recent message.
            _ = tokio::time::sleep(POLL_INTERVAL) => {
                let Some((topic, sub)) = current.as_ref() else { continue };

                // Drain the queue; only the newest message is worth displaying.
                let mut latest = None;
                let mut received = 0u64;
                let mut decode_err = None;
                while let Some(result) = sub.try_recv() {
                    match result {
                        Ok(msg) => {
                            latest = Some(msg);
                            received += 1;
                        }
                        Err(e) => decode_err = Some(e.to_string()),
                    }
                }

                if let Some(msg) = latest {
                    let formatted = format_message_pretty(&msg);
                    let mut s = state.lock();
                    if s.topic.as_deref() == Some(topic.as_str()) {
                        s.msg_count += received;
                        let count = s.msg_count;
                        s.push_line(format!("──────── #{count} ────────"));
                        for line in formatted.lines() {
                            s.push_line(line.to_string());
                        }
                        s.status = EchoStatus::Active;
                        s.last_update = Some(Instant::now());
                    }
                } else if let Some(e) = decode_err {
                    let mut s = state.lock();
                    if s.topic.as_deref() == Some(topic.as_str()) {
                        s.status = EchoStatus::Error(e);
                    }
                }
            }
        }
    }
}
