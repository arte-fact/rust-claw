use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tokio::sync::broadcast;

use crate::protocol::ids::AgentGroupId;

/// How many recent activity events the live feed keeps.
pub const FEED_CAPACITY: usize = 200;
const STREAM_CAPACITY: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Idle,
    Running,
    Failed,
}

/// The current state of one agent, as shown on the activity board (M16).
#[derive(Debug, Clone, Serialize)]
pub struct AgentActivity {
    pub agent: String,
    pub status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delegated_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// RFC3339 start time of the current run, for a client-side elapsed timer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
}

/// What the supervisor knows when a run begins; turned into an [`AgentActivity`].
pub struct RunContext {
    pub agent_group_id: AgentGroupId,
    pub agent: String,
    pub chat: Option<String>,
    pub delegated_by: Option<String>,
    pub message: Option<String>,
}

/// One entry in the activity timeline.
#[derive(Debug, Clone, Serialize)]
pub struct ActivityEvent {
    pub ts: String,
    pub agent: String,
    pub kind: &'static str,
    pub text: String,
}

/// Broadcast unit: the agent that changed (board card) plus the feed line.
#[derive(Debug, Clone, Serialize)]
pub struct ActivityUpdate {
    pub agent_id: String,
    pub activity: AgentActivity,
    pub event: ActivityEvent,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentSlot {
    pub agent_id: String,
    pub activity: AgentActivity,
}

#[derive(Debug, Serialize)]
pub struct ActivitySnapshot {
    pub agents: Vec<AgentSlot>,
    pub feed: Vec<ActivityEvent>,
}

/// In-memory record of what each agent is doing, plus a feed of recent events,
/// fed by the supervisor's run lifecycle and read by the `/admin/activity` board
/// + its SSE stream (M16). Presentation-only: never affects a run.
pub struct ActivityHub {
    agents: Mutex<HashMap<AgentGroupId, AgentActivity>>,
    feed: Mutex<VecDeque<ActivityEvent>>,
    tx: broadcast::Sender<ActivityUpdate>,
    feed_capacity: usize,
}

impl ActivityHub {
    #[must_use]
    pub fn new() -> Arc<Self> {
        let (tx, _) = broadcast::channel(STREAM_CAPACITY);
        Arc::new(Self {
            agents: Mutex::new(HashMap::new()),
            feed: Mutex::new(VecDeque::new()),
            tx,
            feed_capacity: FEED_CAPACITY,
        })
    }

    pub fn started(&self, ctx: &RunContext) {
        let activity = AgentActivity {
            agent: ctx.agent.clone(),
            status: Status::Running,
            chat: ctx.chat.clone(),
            delegated_by: ctx.delegated_by.clone(),
            phase: Some("thinking…".to_owned()),
            message: ctx.message.clone(),
            started_at: Some(now()),
        };
        let where_ = ctx.chat.clone().unwrap_or_else(|| "background".to_owned());
        self.record(
            &ctx.agent_group_id,
            activity,
            "started",
            format!("started · {where_}"),
        );
    }

    pub fn phase(&self, agent_group_id: &AgentGroupId, agent: &str, text: &str) {
        let Some(activity) = self.update(agent_group_id, |a| a.phase = Some(text.to_owned()))
        else {
            return;
        };
        self.emit(
            agent_group_id,
            activity,
            ActivityEvent {
                ts: now(),
                agent: agent.to_owned(),
                kind: "phase",
                text: text.to_owned(),
            },
        );
    }

    /// Clears a *running* agent back to idle. A no-op if it's already `Failed`
    /// (a turn failure already reported it) so the failure stays visible.
    pub fn finished(&self, agent_group_id: &AgentGroupId, agent: &str) {
        let idled = {
            let Ok(mut map) = self.agents.lock() else {
                return;
            };
            match map.get_mut(agent_group_id) {
                Some(a) if a.status == Status::Running => {
                    a.status = Status::Idle;
                    a.phase = None;
                    a.message = None;
                    a.chat = None;
                    a.delegated_by = None;
                    a.started_at = None;
                    Some(a.clone())
                }
                _ => None,
            }
        };
        if let Some(activity) = idled {
            self.emit(
                agent_group_id,
                activity,
                ActivityEvent {
                    ts: now(),
                    agent: agent.to_owned(),
                    kind: "finished",
                    text: "done".to_owned(),
                },
            );
        }
    }

    pub fn failed(&self, agent_group_id: &AgentGroupId, agent: &str, detail: &str) {
        let Some(activity) = self.update(agent_group_id, |a| {
            a.status = Status::Failed;
            a.phase = Some(detail.to_owned());
            a.started_at = None;
        }) else {
            // No prior `started` (e.g. a pre-run resolution failure) — record one.
            let activity = AgentActivity {
                agent: agent.to_owned(),
                status: Status::Failed,
                chat: None,
                delegated_by: None,
                phase: Some(detail.to_owned()),
                message: None,
                started_at: None,
            };
            self.record(
                agent_group_id,
                activity,
                "failed",
                format!("failed · {detail}"),
            );
            return;
        };
        self.emit(
            agent_group_id,
            activity,
            ActivityEvent {
                ts: now(),
                agent: agent.to_owned(),
                kind: "failed",
                text: format!("failed · {detail}"),
            },
        );
    }

    #[must_use]
    pub fn snapshot(&self) -> ActivitySnapshot {
        let agents = self
            .agents
            .lock()
            .map(|map| {
                map.iter()
                    .map(|(id, activity)| AgentSlot {
                        agent_id: id.as_str().to_owned(),
                        activity: activity.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let feed = self
            .feed
            .lock()
            .map(|feed| feed.iter().rev().cloned().collect())
            .unwrap_or_default();
        ActivitySnapshot { agents, feed }
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<ActivityUpdate> {
        self.tx.subscribe()
    }

    /// Replaces an agent's state wholesale (start / cold failure) + feed + broadcast.
    fn record(
        &self,
        agent_group_id: &AgentGroupId,
        activity: AgentActivity,
        kind: &'static str,
        text: String,
    ) {
        if let Ok(mut map) = self.agents.lock() {
            map.insert(agent_group_id.clone(), activity.clone());
        }
        self.emit(
            agent_group_id,
            activity.clone(),
            ActivityEvent {
                ts: now(),
                agent: activity.agent,
                kind,
                text,
            },
        );
    }

    /// Mutates an existing agent's state in place; `None` if it isn't tracked.
    fn update(
        &self,
        agent_group_id: &AgentGroupId,
        mutate: impl FnOnce(&mut AgentActivity),
    ) -> Option<AgentActivity> {
        let mut map = self.agents.lock().ok()?;
        let activity = map.get_mut(agent_group_id)?;
        mutate(activity);
        Some(activity.clone())
    }

    fn emit(&self, agent_group_id: &AgentGroupId, activity: AgentActivity, event: ActivityEvent) {
        if let Ok(mut feed) = self.feed.lock() {
            feed.push_back(event.clone());
            while feed.len() > self.feed_capacity {
                feed.pop_front();
            }
        }
        let _ = self.tx.send(ActivityUpdate {
            agent_id: agent_group_id.as_str().to_owned(),
            activity,
            event,
        });
    }
}

fn now() -> String {
    jiff::Timestamp::now().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(agent: &str) -> RunContext {
        RunContext {
            agent_group_id: AgentGroupId::new(format!("ag-{agent}")),
            agent: agent.to_owned(),
            chat: Some("Main".to_owned()),
            delegated_by: None,
            message: Some("hello".to_owned()),
        }
    }

    #[test]
    fn lifecycle_moves_an_agent_through_running_and_back_to_idle() {
        let hub = ActivityHub::new();
        let id = AgentGroupId::new("ag-andy");

        hub.started(&ctx("andy"));
        let running = &hub.snapshot().agents[0].activity;
        assert_eq!(running.status, Status::Running);
        assert_eq!(running.chat.as_deref(), Some("Main"));
        assert_eq!(running.message.as_deref(), Some("hello"));
        assert!(running.started_at.is_some());

        hub.phase(&id, "andy", "running a command");
        assert_eq!(
            hub.snapshot().agents[0].activity.phase.as_deref(),
            Some("running a command")
        );

        hub.finished(&id, "andy");
        let idle = &hub.snapshot().agents[0].activity;
        assert_eq!(idle.status, Status::Idle);
        assert_eq!(idle.phase, None);

        // started → phase → finished all landed in the feed.
        let kinds: Vec<_> = hub.snapshot().feed.iter().map(|e| e.kind).collect();
        assert_eq!(kinds, ["finished", "phase", "started"]); // newest-first
    }

    #[test]
    fn failure_without_a_prior_start_is_still_recorded() {
        let hub = ActivityHub::new();
        let id = AgentGroupId::new("ag-andy");
        hub.failed(&id, "andy", "no endpoint configured");
        let slot = &hub.snapshot().agents[0].activity;
        assert_eq!(slot.status, Status::Failed);
        assert!(slot.phase.as_deref().unwrap().contains("no endpoint"));
    }

    #[tokio::test]
    async fn subscribers_receive_updates() {
        let hub = ActivityHub::new();
        let mut rx = hub.subscribe();
        hub.started(&ctx("andy"));
        let update = rx.recv().await.expect("update");
        assert_eq!(update.agent_id, "ag-andy");
        assert_eq!(update.activity.status, Status::Running);
        assert_eq!(update.event.kind, "started");
    }

    #[test]
    fn feed_is_capped() {
        let hub = ActivityHub::new();
        let id = AgentGroupId::new("ag-andy");
        hub.started(&ctx("andy"));
        for n in 0..FEED_CAPACITY + 50 {
            hub.phase(&id, "andy", &format!("step {n}"));
        }
        assert!(hub.snapshot().feed.len() <= FEED_CAPACITY);
    }
}
