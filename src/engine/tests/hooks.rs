use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;

use evotengine::provider::mock::MockResponse;
use evotengine::provider::mock::MockToolCall;
use evotengine::provider::MockProvider;
use evotengine::*;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

struct FixedHookExecutor {
    outcome: HookOutcome,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl HookExecutor for FixedHookExecutor {
    async fn execute(
        &self,
        _handler: &HookHandler,
        _invocation: &HookInvocation,
        _timeout: std::time::Duration,
    ) -> std::result::Result<HookOutcome, HookError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.outcome.clone())
    }
}

struct RecordingHookExecutor {
    calls: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl HookExecutor for RecordingHookExecutor {
    async fn execute(
        &self,
        handler: &HookHandler,
        _invocation: &HookInvocation,
        _timeout: std::time::Duration,
    ) -> std::result::Result<HookOutcome, HookError> {
        let mut calls = self
            .calls
            .lock()
            .map_err(|e| HookError::Spawn(e.to_string()))?;
        calls.push(handler.command.clone());
        if handler.command == "fail" {
            return Err(HookError::Status {
                status: "1".to_string(),
                stderr: "lifecycle failed".to_string(),
            });
        }
        Ok(HookOutcome::Allow)
    }
}

struct CountingTool {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl AgentTool for CountingTool {
    fn name(&self) -> &str {
        "mock_tool"
    }

    fn label(&self) -> &str {
        "Mock Tool"
    }

    fn description(&self) -> &str {
        "Mock tool"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    async fn execute(
        &self,
        _params: serde_json::Value,
        _ctx: ToolContext,
    ) -> std::result::Result<ToolResult, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult {
            content: vec![Content::Text {
                text: "tool ran".to_string(),
            }],
            details: serde_json::Value::Null,
            retention: Retention::Normal,
        })
    }
}

fn hook_manager(
    outcome: HookOutcome,
    calls: Arc<AtomicUsize>,
) -> std::result::Result<Arc<HookManager>, Box<dyn std::error::Error>> {
    let config = HookConfig {
        enabled: true,
        timeout_ms: 1_000,
        handlers: vec![HookHandler {
            event: HookEvent::BeforeTool,
            tool: Some("mock_tool".to_string()),
            command: "unused".to_string(),
        }],
    };
    let manager =
        HookManager::with_executor(config, Arc::new(FixedHookExecutor { outcome, calls }))
            .ok_or_else(|| std::io::Error::other("hook manager disabled"))?;
    Ok(Arc::new(manager))
}

fn make_config(provider: MockProvider, hook_manager: Arc<HookManager>) -> AgentLoopConfig {
    AgentLoopConfig {
        provider: Arc::new(provider),
        model: "mock".into(),
        api_key: "test".into(),
        thinking_level: ThinkingLevel::Off,
        max_tokens: None,
        temperature: None,
        model_config: None,
        convert_to_llm: None,
        transform_context: None,
        get_steering_messages: None,
        get_follow_up_messages: None,
        context_config: None,
        compaction_strategy: None,
        execution_limits: None,
        cache_config: CacheConfig::default(),
        tool_execution: ToolExecutionStrategy::Sequential,
        retry_policy: RetryPolicy::disabled(),
        before_turn: None,
        after_turn: None,
        input_filters: Vec::new(),
        spill: None,
        hook_manager: Some(hook_manager),
        hook_context: Some(HookRunContext {
            run_id: "run-test".to_string(),
            session_id: "session-test".to_string(),
        }),
    }
}

async fn run_tool_loop(config: AgentLoopConfig, tool_calls: Arc<AtomicUsize>) -> Vec<AgentEvent> {
    let mut context = AgentContext {
        system_prompt: "test".to_string(),
        messages: Vec::new(),
        tools: vec![Box::new(CountingTool { calls: tool_calls })],
        cwd: std::path::PathBuf::new(),
        path_guard: Arc::new(PathGuard::open()),
    };
    let (tx, mut rx) = mpsc::unbounded_channel();
    let prompt = AgentMessage::Llm(Message::user("run tool"));
    let _messages = agent_loop(
        vec![prompt],
        &mut context,
        &config,
        tx,
        CancellationToken::new(),
    )
    .await;

    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    events
}

#[tokio::test]
async fn before_tool_allow_runs_tool() -> TestResult {
    let hook_calls = Arc::new(AtomicUsize::new(0));
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let manager = hook_manager(HookOutcome::Allow, hook_calls.clone())?;
    let provider = MockProvider::new(vec![
        MockResponse::ToolCalls(vec![MockToolCall {
            name: "mock_tool".to_string(),
            arguments: serde_json::json!({}),
        }]),
        MockResponse::Text("done".to_string()),
    ]);

    let config = make_config(provider, manager);
    let _events = run_tool_loop(config, tool_calls.clone()).await;

    assert_eq!(hook_calls.load(Ordering::SeqCst), 1);
    assert_eq!(tool_calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn before_tool_block_skips_tool_and_returns_error() -> TestResult {
    let hook_calls = Arc::new(AtomicUsize::new(0));
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let manager = hook_manager(
        HookOutcome::Block {
            reason: "blocked by test hook".to_string(),
        },
        hook_calls.clone(),
    )?;
    let provider = MockProvider::new(vec![
        MockResponse::ToolCalls(vec![MockToolCall {
            name: "mock_tool".to_string(),
            arguments: serde_json::json!({}),
        }]),
        MockResponse::Text("done".to_string()),
    ]);

    let config = make_config(provider, manager);
    let events = run_tool_loop(config, tool_calls.clone()).await;

    let blocked = events.iter().any(|event| {
        matches!(
            event,
            AgentEvent::ToolExecutionEnd {
                is_error: true,
                result,
                ..
            } if result.content.iter().any(|c| {
                matches!(c, Content::Text { text } if text.contains("blocked by test hook"))
            })
        )
    });
    assert_eq!(hook_calls.load(Ordering::SeqCst), 1);
    assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
    assert!(blocked);
    Ok(())
}

#[tokio::test]
async fn shell_hook_executor_sends_json_on_stdin() -> TestResult {
    let temp = tempfile::tempdir()?;
    let input_path = temp.path().join("input.json");
    let script_path = temp.path().join("hook.sh");
    let script = format!(
        "cat > '{}'\nprintf '{{\"decision\":\"allow\"}}'\n",
        input_path.display()
    );
    std::fs::write(&script_path, script)?;

    let handler = HookHandler {
        event: HookEvent::BeforeTool,
        tool: Some("*".to_string()),
        command: format!("sh {}", script_path.display()),
    };
    let invocation = HookInvocation {
        event: HookEvent::BeforeTool,
        cwd: temp.path().to_path_buf(),
        run_id: "run".to_string(),
        session_id: "session".to_string(),
        turn: 2,
        tool: Some("bash".to_string()),
        payload: serde_json::json!({"input": {"command": "date"}}),
    };
    let executor = ShellHookExecutor;

    let outcome = executor
        .execute(&handler, &invocation, std::time::Duration::from_secs(1))
        .await?;
    let input = std::fs::read_to_string(input_path)?;
    let parsed: serde_json::Value = serde_json::from_str(&input)?;

    assert_eq!(outcome, HookOutcome::Allow);
    assert_eq!(parsed["event"], "before_tool");
    assert_eq!(parsed["tool"], "bash");
    Ok(())
}

#[tokio::test]
async fn shell_hook_executor_blocks_invalid_decision_json() -> TestResult {
    let temp = tempfile::tempdir()?;
    let handler = HookHandler {
        event: HookEvent::BeforeTool,
        tool: Some("*".to_string()),
        command: "printf not-json".to_string(),
    };
    let invocation = HookInvocation {
        event: HookEvent::BeforeTool,
        cwd: temp.path().to_path_buf(),
        run_id: "run".to_string(),
        session_id: "session".to_string(),
        turn: 0,
        tool: Some("bash".to_string()),
        payload: serde_json::json!({}),
    };
    let executor = ShellHookExecutor;

    let result = executor
        .execute(&handler, &invocation, std::time::Duration::from_secs(1))
        .await;

    assert!(matches!(result, Err(HookError::InvalidJson(_))));
    Ok(())
}

#[tokio::test]
async fn lifecycle_handlers_run_in_order_and_continue_after_error() -> TestResult {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let config = HookConfig {
        enabled: true,
        timeout_ms: 1_000,
        handlers: vec![
            HookHandler {
                event: HookEvent::RunFinished,
                tool: None,
                command: "first".to_string(),
            },
            HookHandler {
                event: HookEvent::RunFinished,
                tool: None,
                command: "fail".to_string(),
            },
            HookHandler {
                event: HookEvent::RunFinished,
                tool: None,
                command: "last".to_string(),
            },
        ],
    };
    let manager = HookManager::with_executor(
        config,
        Arc::new(RecordingHookExecutor {
            calls: calls.clone(),
        }),
    )
    .ok_or_else(|| std::io::Error::other("hook manager disabled"))?;

    let outcome = manager
        .invoke(HookInvocation {
            event: HookEvent::RunFinished,
            cwd: std::path::PathBuf::new(),
            run_id: "run".to_string(),
            session_id: "session".to_string(),
            turn: 1,
            tool: None,
            payload: serde_json::json!({"text": "done"}),
        })
        .await;

    let recorded = calls
        .lock()
        .map_err(|e| std::io::Error::other(e.to_string()))?
        .clone();
    assert_eq!(outcome, HookOutcome::Allow);
    assert_eq!(recorded, vec![
        "first".to_string(),
        "fail".to_string(),
        "last".to_string()
    ]);
    Ok(())
}
