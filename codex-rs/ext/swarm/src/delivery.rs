//! Wakes parked agents: hands each recipient the board messages that mention it in a new turn.

use std::sync::Arc;
use std::sync::Weak;

use codex_core::StartIfIdleSubmission;
use codex_core::ThreadManager;
use codex_core::TurnInput;
use codex_core::TurnInputRequest;
use codex_core::TurnStartOptions;
use codex_core::context::InternalContextSource;
use codex_core::context::InternalModelContextFragment;
use codex_extension_api::ContextualUserFragment;
use codex_protocol::ThreadId;

use crate::prompts::delivery_prompt;
use crate::service::SwarmRuntime;
use crate::state::COORDINATOR;

const CONTEXT_SOURCE: &str = "swarm";
const TURN_TRIGGER: &str = "swarm";

pub(crate) fn context_fragment(prompt: String) -> InternalModelContextFragment {
    InternalModelContextFragment::new(InternalContextSource::from_static(CONTEXT_SOURCE), prompt)
}

/// Starts turns on other threads of the same thread manager.
pub(crate) struct Courier {
    thread_manager: Weak<ThreadManager>,
}

impl Courier {
    pub(crate) fn new(thread_manager: Weak<ThreadManager>) -> Self {
        Self { thread_manager }
    }

    /// Starts a turn on `thread_id` with `prompt` if the thread is idle. Returns whether a turn
    /// started. A busy thread gets the messages when its turn ends, from the idle hook.
    pub(crate) async fn wake(&self, thread_id: &str, prompt: String) -> bool {
        let Some(thread_manager) = self.thread_manager.upgrade() else {
            return false;
        };
        let Ok(thread_id) = ThreadId::from_string(thread_id) else {
            return false;
        };
        let Ok(thread) = thread_manager.get_thread(thread_id).await else {
            return false;
        };
        let item = ContextualUserFragment::into(context_fragment(prompt));
        let request =
            TurnInputRequest::new(TurnInput::ResponseItem(item)).on_start(TurnStartOptions {
                turn_trigger: Some(TURN_TRIGGER.to_string()),
                ..Default::default()
            });
        match thread.start_turn_if_idle(request).await {
            Ok(StartIfIdleSubmission::Started { .. }) => true,
            Ok(StartIfIdleSubmission::NotSubmitted { reason }) => {
                tracing::debug!(?reason, "swarm delivery was not submitted");
                false
            }
            Err(error) => {
                tracing::debug!(%error, "swarm delivery failed to submit");
                false
            }
        }
    }

    /// Delivers pending messages to each named recipient that has a thread. Returns the names of
    /// the agents that were woken.
    pub(crate) async fn deliver(
        &self,
        runtime: &Arc<SwarmRuntime>,
        names: &[String],
    ) -> Vec<String> {
        let mut woken = Vec::new();
        for name in names {
            let plan = runtime.with_run(|run| {
                let run = run.as_ref()?;
                let pending = run.pending_for(name);
                let upto = pending.last()?.seq;
                let thread_id = if name == COORDINATOR {
                    run.coordinator_thread_id.clone()
                } else {
                    run.member(name)?.thread_id.clone()?
                };
                Some((thread_id, upto, delivery_prompt(run, name, &pending)))
            });
            let Some((thread_id, upto, prompt)) = plan else {
                continue;
            };
            if self.wake(&thread_id, prompt).await {
                runtime.with_run(|run| {
                    if let Some(run) = run.as_mut() {
                        run.mark_delivered(name, upto);
                        run.mark_active(name);
                    }
                });
                woken.push(name.clone());
            }
        }
        if !woken.is_empty() {
            runtime.save().await;
        }
        woken
    }
}
