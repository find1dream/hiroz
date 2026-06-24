//! Live message echo for the Measure panel.
//!
//! A single background task owns one dynamic subscriber per *measured* topic and
//! decodes each topic's latest message into a per-topic [`EchoState`], much like
//! `ros2 topic echo <topic>`. Every measured topic is echoed continuously in the
//! background, so switching the selected topic in the Measure panel just changes
//! which buffer is displayed — history keeps accumulating for all of them.
//! Schema discovery and CDR deserialization happen off the render loop so the UI
//! never blocks.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::{mpsc, watch};

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

/// Background task: keep one subscriber + buffer per measured topic up to date.
///
/// The set of topics to echo arrives over `topics_rx`; the worker creates a
/// subscriber for each new topic and drops the subscriber + buffer for any topic
/// no longer measured. Every subscribed topic is drained on each poll tick, so
/// all measured topics accumulate history concurrently regardless of which one
/// is currently selected in the UI.
///
/// Returns when the watch sender is dropped (i.e. the app is shutting down).
pub async fn run_echo_worker(
    core: Arc<CoreEngine>,
    states: Arc<Mutex<HashMap<String, EchoState>>>,
    mut topics_rx: watch::Receiver<Vec<String>>,
) {
    // Active subscribers, keyed by topic.
    let mut current: HashMap<String, DynSub> = HashMap::new();
    // Topics with an in-flight discovery task, so we don't spawn duplicates.
    let mut pending: HashSet<String> = HashSet::new();
    // Discovery results flow back here so a slow/missing topic never blocks the
    // poll loop for the topics that are already live.
    let (sub_tx, mut sub_rx) =
        mpsc::unbounded_channel::<(String, Result<DynSub, String>)>();

    loop {
        tokio::select! {
            // The set of measured topics changed (or the app is shutting down).
            changed = topics_rx.changed() => {
                if changed.is_err() {
                    return; // sender dropped
                }
                let desired = topics_rx.borrow_and_update().clone();
                let desired_set: HashSet<&String> = desired.iter().collect();

                // Drop subscribers / discoveries / buffers for unmeasured topics.
                current.retain(|t, _| desired_set.contains(t));
                pending.retain(|t| desired_set.contains(t));
                {
                    let mut s = states.lock();
                    s.retain(|t, _| desired_set.contains(t));
                    // Seed a placeholder buffer for any newly-added topic.
                    for topic in &desired {
                        s.entry(topic.clone()).or_insert_with(|| {
                            let mut st = EchoState::default();
                            st.reset_for(Some(topic.clone()), EchoStatus::Discovering);
                            st
                        });
                    }
                }

                // Kick off discovery for topics we aren't subscribed to yet.
                for topic in &desired {
                    if !current.contains_key(topic) && pending.insert(topic.clone()) {
                        let core = core.clone();
                        let sub_tx = sub_tx.clone();
                        let topic = topic.clone();
                        tokio::spawn(async move {
                            let res = core
                                .create_echo_subscriber(&topic, DISCOVERY_TIMEOUT)
                                .await
                                .map_err(|e| e.to_string());
                            let _ = sub_tx.send((topic, res));
                        });
                    }
                }
            }

            // A discovery task finished — adopt the subscriber or record the error.
            Some((topic, res)) = sub_rx.recv() => {
                pending.remove(&topic);
                let mut s = states.lock();
                // Ignore results for topics dropped while discovering.
                let Some(st) = s.get_mut(&topic) else { continue };
                match res {
                    Ok(sub) => {
                        st.type_name = sub.schema().map(|sch| sch.type_name.clone());
                        if st.status == EchoStatus::Discovering {
                            st.status = EchoStatus::NoData;
                        }
                        drop(s);
                        current.insert(topic, sub);
                    }
                    Err(e) => {
                        st.status = EchoStatus::Error(e);
                    }
                }
            }

            // Periodically drain every subscriber and append the newest message.
            _ = tokio::time::sleep(POLL_INTERVAL) => {
                for (topic, sub) in current.iter() {
                    // Drain the queue; only the newest message is worth displaying,
                    // but every message still counts toward the running total.
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

                    if latest.is_none() && decode_err.is_none() {
                        continue;
                    }
                    let formatted = latest.as_ref().map(format_message_pretty);

                    let mut s = states.lock();
                    let Some(st) = s.get_mut(topic) else { continue };
                    if let Some(formatted) = formatted {
                        st.msg_count += received;
                        let count = st.msg_count;
                        st.push_line(format!("──────── #{count} ────────"));
                        for line in formatted.lines() {
                            st.push_line(line.to_string());
                        }
                        st.status = EchoStatus::Active;
                        st.last_update = Some(Instant::now());
                    } else if let Some(e) = decode_err {
                        st.status = EchoStatus::Error(e);
                    }
                }
            }
        }
    }
}
