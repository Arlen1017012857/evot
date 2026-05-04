//! Tool execution: sequential, batch, and single-tool dispatch.

use std::sync::Arc;

use tokio::sync::mpsc;

use super::config::GetMessagesFn;
use crate::context;
use crate::hooks::HookInvocation;
use crate::hooks::HookManager;
use crate::hooks::HookOutcome;
use crate::hooks::HookRunContext;
use crate::spill::FsSpill;
use crate::tools::guard::PathGuard;
use crate::types::*;

pub(super) struct ToolExecutionResult {
    pub tool_results: Vec<Message>,
    pub steering_messages: Option<Vec<AgentMessage>>,
}

/// Check if all tool calls in a batch are concurrency-safe.
fn all_concurrency_safe(
    tools: &[Box<dyn AgentTool>],
    tool_calls: &[(String, String, serde_json::Value)],
) -> bool {
    tool_calls.iter().all(|(_, name, _)| {
        tools
            .iter()
            .find(|t| t.name() == name)
            .map(|t| t.is_concurrency_safe())
            .unwrap_or(false)
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn execute_tool_calls(
    tools: &[Box<dyn AgentTool>],
    tool_calls: &[(String, String, serde_json::Value)],
    tx: &mpsc::UnboundedSender<AgentEvent>,
    cancel: &tokio_util::sync::CancellationToken,
    get_steering: Option<&GetMessagesFn>,
    strategy: &ToolExecutionStrategy,
    cwd: &std::path::Path,
    path_guard: &Arc<PathGuard>,
    spill: &Option<Arc<FsSpill>>,
    hook_manager: &Option<Arc<HookManager>>,
    hook_context: &Option<HookRunContext>,
    turn: usize,
) -> ToolExecutionResult {
    match strategy {
        ToolExecutionStrategy::Sequential => {
            execute_sequential(
                tools,
                tool_calls,
                tx,
                cancel,
                get_steering,
                cwd,
                path_guard,
                spill,
                hook_manager,
                hook_context,
                turn,
            )
            .await
        }
        ToolExecutionStrategy::Parallel => {
            if all_concurrency_safe(tools, tool_calls) {
                execute_batch(
                    tools,
                    tool_calls,
                    tx,
                    cancel,
                    get_steering,
                    cwd,
                    path_guard,
                    spill,
                    hook_manager,
                    hook_context,
                    turn,
                )
                .await
            } else {
                execute_sequential(
                    tools,
                    tool_calls,
                    tx,
                    cancel,
                    get_steering,
                    cwd,
                    path_guard,
                    spill,
                    hook_manager,
                    hook_context,
                    turn,
                )
                .await
            }
        }
        ToolExecutionStrategy::Batched { size } => {
            let mut results: Vec<Message> = Vec::new();
            let mut steering_messages: Option<Vec<AgentMessage>> = None;

            for (batch_idx, batch) in tool_calls.chunks(*size).enumerate() {
                let batch_result = if all_concurrency_safe(tools, batch) {
                    execute_batch(
                        tools,
                        batch,
                        tx,
                        cancel,
                        None,
                        cwd,
                        path_guard,
                        spill,
                        hook_manager,
                        hook_context,
                        turn,
                    )
                    .await
                } else {
                    execute_sequential(
                        tools,
                        batch,
                        tx,
                        cancel,
                        None,
                        cwd,
                        path_guard,
                        spill,
                        hook_manager,
                        hook_context,
                        turn,
                    )
                    .await
                };
                results.extend(batch_result.tool_results);

                // Check steering between batches
                if let Some(get_steering_fn) = get_steering {
                    let steering = get_steering_fn();
                    if !steering.is_empty() {
                        steering_messages = Some(steering);
                        // Skip remaining batches
                        let executed = (batch_idx + 1) * *size;
                        if executed < tool_calls.len() {
                            for (skip_id, skip_name, _) in &tool_calls[executed..] {
                                results.push(skip_tool_call(skip_id, skip_name, tx));
                            }
                        }
                        break;
                    }
                }
            }

            ToolExecutionResult {
                tool_results: results,
                steering_messages,
            }
        }
    }
}

/// Execute tool calls one at a time, checking steering between each.
#[allow(clippy::too_many_arguments)]
async fn execute_sequential(
    tools: &[Box<dyn AgentTool>],
    tool_calls: &[(String, String, serde_json::Value)],
    tx: &mpsc::UnboundedSender<AgentEvent>,
    cancel: &tokio_util::sync::CancellationToken,
    get_steering: Option<&GetMessagesFn>,
    cwd: &std::path::Path,
    path_guard: &Arc<PathGuard>,
    spill: &Option<Arc<FsSpill>>,
    hook_manager: &Option<Arc<HookManager>>,
    hook_context: &Option<HookRunContext>,
    turn: usize,
) -> ToolExecutionResult {
    let mut results: Vec<Message> = Vec::new();
    let mut steering_messages: Option<Vec<AgentMessage>> = None;

    for (index, (id, name, args)) in tool_calls.iter().enumerate() {
        let (msg, _is_error) = execute_single_tool(
            tools,
            id,
            name,
            args,
            tx,
            cancel,
            cwd,
            path_guard,
            spill,
            hook_manager,
            hook_context,
            turn,
        )
        .await;
        results.push(msg);

        // Check for steering — skip remaining tools if user interrupted
        if let Some(get_steering_fn) = get_steering {
            let steering = get_steering_fn();
            if !steering.is_empty() {
                steering_messages = Some(steering);
                for (skip_id, skip_name, _) in &tool_calls[index + 1..] {
                    results.push(skip_tool_call(skip_id, skip_name, tx));
                }
                break;
            }
        }
    }

    ToolExecutionResult {
        tool_results: results,
        steering_messages,
    }
}

/// Execute a batch of tool calls concurrently using futures::join_all.
#[allow(clippy::too_many_arguments)]
async fn execute_batch(
    tools: &[Box<dyn AgentTool>],
    tool_calls: &[(String, String, serde_json::Value)],
    tx: &mpsc::UnboundedSender<AgentEvent>,
    cancel: &tokio_util::sync::CancellationToken,
    get_steering: Option<&GetMessagesFn>,
    cwd: &std::path::Path,
    path_guard: &Arc<PathGuard>,
    spill: &Option<Arc<FsSpill>>,
    hook_manager: &Option<Arc<HookManager>>,
    hook_context: &Option<HookRunContext>,
    turn: usize,
) -> ToolExecutionResult {
    use futures::future::join_all;

    let futures: Vec<_> = tool_calls
        .iter()
        .map(|(id, name, args)| {
            execute_single_tool(
                tools,
                id,
                name,
                args,
                tx,
                cancel,
                cwd,
                path_guard,
                spill,
                hook_manager,
                hook_context,
                turn,
            )
        })
        .collect();

    let batch_results = join_all(futures).await;

    let results: Vec<Message> = batch_results.into_iter().map(|(msg, _)| msg).collect();

    // Check steering after batch completes
    let steering_messages = if let Some(get_steering_fn) = get_steering {
        let steering = get_steering_fn();
        if steering.is_empty() {
            None
        } else {
            Some(steering)
        }
    } else {
        None
    };

    ToolExecutionResult {
        tool_results: results,
        steering_messages,
    }
}

/// Execute a single tool call and emit events.
#[allow(clippy::too_many_arguments)]
async fn execute_single_tool(
    tools: &[Box<dyn AgentTool>],
    id: &str,
    name: &str,
    args: &serde_json::Value,
    tx: &mpsc::UnboundedSender<AgentEvent>,
    cancel: &tokio_util::sync::CancellationToken,
    cwd: &std::path::Path,
    path_guard: &Arc<PathGuard>,
    spill: &Option<Arc<FsSpill>>,
    hook_manager: &Option<Arc<HookManager>>,
    hook_context: &Option<HookRunContext>,
    turn: usize,
) -> (Message, bool) {
    let tool = tools.iter().find(|t| t.name() == name);

    let preview_command = tool.and_then(|t| t.preview_command(args));

    tx.send(AgentEvent::ToolExecutionStart {
        tool_call_id: id.to_string(),
        tool_name: name.to_string(),
        args: args.clone(),
        preview_command: preview_command.clone(),
    })
    .ok();

    let tool_start = std::time::Instant::now();

    if let Some(block_reason) =
        run_before_tool_hook(hook_manager, hook_context, BeforeToolHookInput {
            cwd,
            id,
            name,
            args,
            preview_command: &preview_command,
            turn,
        })
        .await
    {
        let result = ToolResult {
            content: vec![Content::Text { text: block_reason }],
            details: serde_json::json!({ "hook_blocked": true }),
            retention: Retention::Normal,
        };
        return finish_tool_call(tx, id, name, result, true, tool_start.elapsed()).await;
    }

    let on_update: Option<ToolUpdateFn> = {
        let tx = tx.clone();
        let id = id.to_string();
        let name = name.to_string();
        Some(Arc::new(move |partial: ToolResult| {
            tx.send(AgentEvent::ToolExecutionUpdate {
                tool_call_id: id.clone(),
                tool_name: name.clone(),
                partial_result: partial,
            })
            .ok();
        }))
    };

    let on_progress: Option<ProgressFn> = {
        let tx = tx.clone();
        let id = id.to_string();
        let name = name.to_string();
        Some(Arc::new(move |text: String| {
            tx.send(AgentEvent::ProgressMessage {
                tool_call_id: id.clone(),
                tool_name: name.clone(),
                text,
            })
            .ok();
        }))
    };

    let ctx = ToolContext {
        tool_call_id: id.to_string(),
        tool_name: name.to_string(),
        cancel: cancel.child_token(),
        on_update,
        on_progress,
        cwd: cwd.to_path_buf(),
        path_guard: path_guard.clone(),
    };

    let (result, is_error) = match tool {
        Some(tool) => {
            // Schema pre-validation + type coercion (à la Claude Code / Forge Code).
            let validated_args = crate::tools::validation::validate_and_coerce(
                name,
                &tool.parameters_schema(),
                args,
            );
            match validated_args {
                Err(validation_error) => (
                    ToolResult {
                        content: vec![Content::Text {
                            text: crate::tools::validation::truncate_error(&validation_error),
                        }],
                        details: serde_json::Value::Null,
                        retention: Retention::Normal,
                    },
                    true,
                ),
                Ok(coerced_args) => match tool.execute(coerced_args, ctx).await {
                    Ok(r) => (r, false),
                    Err(e) => (
                        ToolResult {
                            content: vec![Content::Text {
                                text: e.to_string(),
                            }],
                            details: serde_json::Value::Null,
                            retention: Retention::Normal,
                        },
                        true,
                    ),
                },
            }
        }
        None => (
            ToolResult {
                content: vec![Content::Text {
                    text: format!("Tool {} not found", name),
                }],
                details: serde_json::Value::Null,
                retention: Retention::Normal,
            },
            true,
        ),
    };

    // System-level tool result size management.
    // If spill is configured, large results are written to disk with a preview.
    // Otherwise, fall back to truncation.
    let result = process_result(spill, id, name, result, is_error).await;

    finish_tool_call(tx, id, name, result, is_error, tool_start.elapsed()).await
}

async fn finish_tool_call(
    tx: &mpsc::UnboundedSender<AgentEvent>,
    id: &str,
    name: &str,
    result: ToolResult,
    is_error: bool,
    duration: std::time::Duration,
) -> (Message, bool) {
    let result_tokens = context::content_tokens(&result.content);
    let tool_duration_ms = duration.as_millis() as u64;

    tx.send(AgentEvent::ToolExecutionEnd {
        tool_call_id: id.to_string(),
        tool_name: name.to_string(),
        result: result.clone(),
        is_error,
        result_tokens,
        duration_ms: tool_duration_ms,
    })
    .ok();

    let tool_result_msg = Message::ToolResult {
        tool_call_id: id.to_string(),
        tool_name: name.to_string(),
        content: result.content,
        is_error,
        timestamp: now_ms(),
        retention: result.retention,
    };

    tx.send(AgentEvent::MessageStart {
        message: tool_result_msg.clone().into(),
    })
    .ok();
    tx.send(AgentEvent::MessageEnd {
        message: tool_result_msg.clone().into(),
    })
    .ok();

    (tool_result_msg, is_error)
}

struct BeforeToolHookInput<'a> {
    cwd: &'a std::path::Path,
    id: &'a str,
    name: &'a str,
    args: &'a serde_json::Value,
    preview_command: &'a Option<String>,
    turn: usize,
}

async fn run_before_tool_hook(
    hook_manager: &Option<Arc<HookManager>>,
    hook_context: &Option<HookRunContext>,
    input: BeforeToolHookInput<'_>,
) -> Option<String> {
    let (Some(manager), Some(run_context)) = (hook_manager, hook_context) else {
        return None;
    };
    let invocation = HookInvocation {
        event: crate::hooks::HookEvent::BeforeTool,
        cwd: input.cwd.to_path_buf(),
        run_id: run_context.run_id.clone(),
        session_id: run_context.session_id.clone(),
        turn: usize_to_u32(input.turn),
        tool: Some(input.name.to_string()),
        payload: serde_json::json!({
            "tool_call_id": input.id,
            "tool_name": input.name,
            "input": input.args,
            "preview_command": input.preview_command,
        }),
    };
    match manager.invoke(invocation).await {
        HookOutcome::Allow => None,
        HookOutcome::Block { reason } => Some(reason),
    }
}

fn usize_to_u32(value: usize) -> u32 {
    if value > u32::MAX as usize {
        u32::MAX
    } else {
        value as u32
    }
}

pub(super) fn skip_tool_call(
    tool_call_id: &str,
    tool_name: &str,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> Message {
    let result = ToolResult {
        content: vec![Content::Text {
            text: "Skipped due to queued user message.".into(),
        }],
        details: serde_json::Value::Null,
        retention: Retention::Normal,
    };

    tx.send(AgentEvent::ToolExecutionStart {
        tool_call_id: tool_call_id.into(),
        tool_name: tool_name.into(),
        args: serde_json::Value::Null,
        preview_command: None,
    })
    .ok();

    let result_tokens = context::content_tokens(&result.content);

    tx.send(AgentEvent::ToolExecutionEnd {
        tool_call_id: tool_call_id.into(),
        tool_name: tool_name.into(),
        result: result.clone(),
        is_error: true,
        result_tokens,
        duration_ms: 0,
    })
    .ok();

    let msg = Message::ToolResult {
        tool_call_id: tool_call_id.into(),
        tool_name: tool_name.into(),
        content: result.content,
        is_error: true,
        timestamp: now_ms(),
        retention: Retention::Normal,
    };

    tx.send(AgentEvent::MessageStart {
        message: msg.clone().into(),
    })
    .ok();
    tx.send(AgentEvent::MessageEnd {
        message: msg.clone().into(),
    })
    .ok();

    msg
}

pub(super) fn skip_tool_call_doom_loop(
    tool_call_id: &str,
    tool_name: &str,
    args: &serde_json::Value,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> Message {
    let result = ToolResult {
        content: vec![Content::Text {
            text: "Skipped: doom loop detected — repeated identical tool call.".into(),
        }],
        details: serde_json::Value::Null,
        retention: Retention::Normal,
    };

    let preview = build_doom_loop_preview(tool_name, args);

    tx.send(AgentEvent::ToolExecutionStart {
        tool_call_id: tool_call_id.into(),
        tool_name: tool_name.into(),
        args: args.clone(),
        preview_command: Some(preview),
    })
    .ok();

    let result_tokens = context::content_tokens(&result.content);

    tx.send(AgentEvent::ToolExecutionEnd {
        tool_call_id: tool_call_id.into(),
        tool_name: tool_name.into(),
        result: result.clone(),
        is_error: true,
        result_tokens,
        duration_ms: 0,
    })
    .ok();

    let msg = Message::ToolResult {
        tool_call_id: tool_call_id.into(),
        tool_name: tool_name.into(),
        content: result.content,
        is_error: true,
        timestamp: now_ms(),
        retention: Retention::Normal,
    };

    tx.send(AgentEvent::MessageStart {
        message: msg.clone().into(),
    })
    .ok();
    tx.send(AgentEvent::MessageEnd {
        message: msg.clone().into(),
    })
    .ok();

    msg
}

/// Build a compact preview string for doom-loop skipped tool calls.
fn build_doom_loop_preview(tool_name: &str, args: &serde_json::Value) -> String {
    let mut parts = vec![tool_name.to_string()];
    if let serde_json::Value::Object(map) = args {
        for (k, v) in map {
            let val = match v {
                serde_json::Value::String(s) => {
                    if s.len() > 80 {
                        format!("{}…", &s[..80])
                    } else {
                        s.clone()
                    }
                }
                other => {
                    let s = other.to_string();
                    if s.len() > 80 {
                        format!("{}…", &s[..80])
                    } else {
                        s
                    }
                }
            };
            parts.push(format!("{k}={val}"));
        }
    }
    parts.join(" ")
}

// ── Spill / truncation helpers ──────────────────────────────────────────

const PREVIEW_CAP: usize = 4_000;

async fn process_result(
    spill: &Option<Arc<FsSpill>>,
    tool_call_id: &str,
    tool_name: &str,
    result: ToolResult,
    is_error: bool,
) -> ToolResult {
    if is_error {
        return truncate_result(result);
    }

    let spill = match spill {
        Some(s) => s,
        None => return truncate_result(result),
    };

    let text = merge_text_blocks(&result.content);
    if text.is_empty() {
        return result;
    }

    let req = crate::spill::SpillRequest {
        key: tool_call_id.to_string(),
        text,
    };

    match spill.spill(req).await {
        Ok(Some(spill_ref)) => build_spilled_result(result, spill_ref),
        Ok(None) => truncate_result(result),
        Err(e) => {
            tracing::warn!(
                tool_name = tool_name,
                tool_call_id = tool_call_id,
                "spill failed: {e}"
            );
            truncate_result(result)
        }
    }
}

fn build_spilled_result(result: ToolResult, spill_ref: crate::spill::SpillRef) -> ToolResult {
    let preview = if spill_ref.preview.len() > PREVIEW_CAP {
        let boundary = spill_ref.preview.floor_char_boundary(PREVIEW_CAP);
        &spill_ref.preview[..boundary]
    } else {
        &spill_ref.preview
    };

    let msg = format!(
        "Tool output was too large ({} bytes) and was saved to:\n{}\n\n\
         Only a preview is shown below. Use read_file with offset/limit to read the full output.\n\n\
         Preview:\n{}",
        spill_ref.size_bytes,
        spill_ref.path.display(),
        preview,
    );

    let ToolResult {
        content,
        details,
        retention,
    } = result;

    let mut new_content: Vec<Content> = vec![Content::Text { text: msg }];
    for c in content {
        if !matches!(c, Content::Text { .. }) {
            new_content.push(c);
        }
    }

    ToolResult {
        content: new_content,
        details,
        retention,
    }
}

fn truncate_result(result: ToolResult) -> ToolResult {
    let ToolResult {
        content,
        details,
        retention,
    } = result;
    ToolResult {
        content: crate::tools::validation::cap_tool_result_content(
            content,
            crate::tools::validation::MAX_TOOL_RESULT_BYTES,
        ),
        details,
        retention,
    }
}

fn merge_text_blocks(content: &[Content]) -> String {
    let mut merged = String::new();
    for c in content {
        if let Content::Text { text } = c {
            if !merged.is_empty() {
                merged.push('\n');
            }
            merged.push_str(text);
        }
    }
    merged
}
