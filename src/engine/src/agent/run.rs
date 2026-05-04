//! Agent run/resume/finish — submitting work to the agent loop.

use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::agent::Agent;
use super::handle::QueueMode;
use super::handle::RunHandle;
use crate::context::ContextConfig;
use crate::r#loop::agent_loop;
use crate::r#loop::agent_loop_continue;
use crate::r#loop::AgentLoopConfig;
use crate::types::*;

impl Agent {
    // -- Submitting --

    pub async fn submit_text(
        &mut self,
        text: impl Into<String>,
    ) -> (RunHandle, mpsc::UnboundedReceiver<AgentEvent>) {
        let msg = AgentMessage::Llm(Message::user(text));
        self.submit(vec![msg]).await
    }

    pub async fn submit(
        &mut self,
        messages: Vec<AgentMessage>,
    ) -> (RunHandle, mpsc::UnboundedReceiver<AgentEvent>) {
        self.finish().await;

        assert!(
            !self.is_streaming,
            "Agent is already streaming. Use steer() or follow_up()."
        );

        let cancel = CancellationToken::new();
        self.cancel = Some(cancel.clone());
        self.is_streaming = true;

        // Create per-run queues, draining any pre-queued messages
        let run_steering = Arc::new(Mutex::new(self.steering_queue.lock().drain(..).collect()));
        let run_follow_up = Arc::new(Mutex::new(self.follow_up_queue.lock().drain(..).collect()));
        let run_handle = RunHandle {
            steering_queue: run_steering.clone(),
            follow_up_queue: run_follow_up.clone(),
            cancel: cancel.clone(),
        };
        self.last_run_handle = Some(run_handle.clone());

        let (tx, rx) = mpsc::unbounded_channel();

        let mut context = AgentContext {
            system_prompt: self.system_prompt.clone(),
            messages: self.messages.clone(),
            tools: std::mem::take(&mut self.tools),
            cwd: self.cwd.clone(),
            path_guard: self.path_guard.clone(),
        };

        let config = self.build_config_with_queues(run_steering, run_follow_up);

        let handle = tokio::spawn(async move {
            let result = std::panic::AssertUnwindSafe(async {
                let _new_messages =
                    agent_loop(messages, &mut context, &config, tx.clone(), cancel).await;
            });
            if let Err(e) = futures::FutureExt::catch_unwind(result).await {
                let msg = match e.downcast_ref::<&str>() {
                    Some(s) => s.to_string(),
                    None => match e.downcast_ref::<String>() {
                        Some(s) => s.clone(),
                        None => "unknown panic".into(),
                    },
                };
                tx.send(AgentEvent::Error {
                    error: AgentErrorInfo {
                        kind: AgentErrorKind::Runtime,
                        message: format!("Agent loop panicked: {msg}"),
                    },
                })
                .ok();
                tx.send(AgentEvent::AgentEnd { messages: vec![] }).ok();
            }
            (context.tools, context.messages)
        });

        self.pending_completion = Some(handle);
        (run_handle, rx)
    }

    pub async fn resume(&mut self) -> (RunHandle, mpsc::UnboundedReceiver<AgentEvent>) {
        self.finish().await;

        let (tx, rx) = mpsc::unbounded_channel();

        if self.is_streaming {
            tx.send(AgentEvent::Error {
                error: AgentErrorInfo {
                    kind: AgentErrorKind::Runtime,
                    message: "Agent is already streaming, skipping resume".into(),
                },
            })
            .ok();
            return (RunHandle::noop(), rx);
        }
        if self.messages.is_empty() {
            tx.send(AgentEvent::Error {
                error: AgentErrorInfo {
                    kind: AgentErrorKind::Runtime,
                    message: "No messages to resume from, skipping resume".into(),
                },
            })
            .ok();
            return (RunHandle::noop(), rx);
        }

        let cancel = CancellationToken::new();
        self.cancel = Some(cancel.clone());
        self.is_streaming = true;

        let run_steering = Arc::new(Mutex::new(self.steering_queue.lock().drain(..).collect()));
        let run_follow_up = Arc::new(Mutex::new(self.follow_up_queue.lock().drain(..).collect()));
        let run_handle = RunHandle {
            steering_queue: run_steering.clone(),
            follow_up_queue: run_follow_up.clone(),
            cancel: cancel.clone(),
        };
        self.last_run_handle = Some(run_handle.clone());

        let mut context = AgentContext {
            system_prompt: self.system_prompt.clone(),
            messages: self.messages.clone(),
            tools: std::mem::take(&mut self.tools),
            cwd: self.cwd.clone(),
            path_guard: self.path_guard.clone(),
        };

        let config = self.build_config_with_queues(run_steering, run_follow_up);

        let handle = tokio::spawn(async move {
            let result = std::panic::AssertUnwindSafe(async {
                let _new_messages =
                    agent_loop_continue(&mut context, &config, tx.clone(), cancel).await;
            });
            if let Err(e) = futures::FutureExt::catch_unwind(result).await {
                let msg = match e.downcast_ref::<&str>() {
                    Some(s) => s.to_string(),
                    None => match e.downcast_ref::<String>() {
                        Some(s) => s.clone(),
                        None => "unknown panic".into(),
                    },
                };
                tx.send(AgentEvent::Error {
                    error: AgentErrorInfo {
                        kind: AgentErrorKind::Runtime,
                        message: format!("Agent loop panicked: {msg}"),
                    },
                })
                .ok();
                tx.send(AgentEvent::AgentEnd { messages: vec![] }).ok();
            }
            (context.tools, context.messages)
        });

        self.pending_completion = Some(handle);
        (run_handle, rx)
    }

    pub async fn finish(&mut self) {
        if let Some(handle) = self.pending_completion.take() {
            match handle.await {
                Ok((tools, messages)) => {
                    self.tools = tools;
                    self.messages = messages;
                }
                Err(e) => {
                    tracing::error!("Agent loop task failed: {}", e);
                }
            }
            self.is_streaming = false;
            self.cancel = None;
            self.last_run_handle = None;
        }
    }

    // -- Internal --

    fn build_config_with_queues(
        &self,
        steering_queue: Arc<Mutex<Vec<AgentMessage>>>,
        follow_up_queue: Arc<Mutex<Vec<AgentMessage>>>,
    ) -> AgentLoopConfig {
        let steering_mode = self.steering_mode;
        let follow_up_mode = self.follow_up_mode;

        AgentLoopConfig {
            provider: self.provider.clone(),
            model: self.model.clone(),
            api_key: self.api_key.clone(),
            thinking_level: self.thinking_level,
            max_tokens: self.max_tokens,
            temperature: self.temperature,
            model_config: self.model_config.clone(),
            convert_to_llm: None,
            transform_context: None,
            get_steering_messages: Some(Box::new(move || {
                let mut queue = steering_queue.lock();
                match steering_mode {
                    QueueMode::OneAtATime => {
                        if queue.is_empty() {
                            vec![]
                        } else {
                            vec![queue.remove(0)]
                        }
                    }
                    QueueMode::All => queue.drain(..).collect(),
                }
            })),
            context_config: if self.context_management_disabled {
                None
            } else {
                Some(self.context_config.clone().unwrap_or_else(|| {
                    self.model_config
                        .as_ref()
                        .map(|m| ContextConfig::from_context_window(m.context_window))
                        .unwrap_or_default()
                }))
            },
            compaction_strategy: self.compaction_strategy.clone(),
            execution_limits: self.execution_limits.clone(),
            cache_config: self.cache_config.clone(),
            tool_execution: self.tool_execution.clone(),
            retry_policy: self.retry_policy.clone(),
            get_follow_up_messages: Some(Box::new(move || {
                let mut queue = follow_up_queue.lock();
                match follow_up_mode {
                    QueueMode::OneAtATime => {
                        if queue.is_empty() {
                            vec![]
                        } else {
                            vec![queue.remove(0)]
                        }
                    }
                    QueueMode::All => queue.drain(..).collect(),
                }
            })),
            before_turn: self.before_turn.clone(),
            after_turn: self.after_turn.clone(),
            input_filters: self.input_filters.clone(),
            spill: self.spill.clone(),
            hook_manager: self.hook_manager.clone(),
            hook_context: self.hook_context.clone(),
        }
    }
}
