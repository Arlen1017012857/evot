use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde::Serialize;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub handlers: Vec<HookHandler>,
}

impl HookConfig {
    pub fn enabled_with_handlers(&self) -> bool {
        self.enabled && !self.handlers.is_empty()
    }
}

impl Default for HookConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            timeout_ms: default_timeout_ms(),
            handlers: Vec::new(),
        }
    }
}

fn default_timeout_ms() -> u64 {
    5_000
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookHandler {
    pub event: HookEvent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    pub command: String,
}

impl HookHandler {
    fn matches(&self, event: HookEvent, tool: Option<&str>) -> bool {
        if self.event != event {
            return false;
        }
        match (&self.tool, tool) {
            (None, _) => true,
            (Some(pattern), _) if pattern == "*" => true,
            (Some(pattern), Some(name)) => pattern == name,
            (Some(_), None) => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    BeforeTool,
    RunStarted,
    TurnStarted,
    AssistantCompleted,
    ToolStarted,
    ToolFinished,
    LlmCallStarted,
    LlmCallCompleted,
    ContextCompactionStarted,
    ContextCompactionCompleted,
    RunFinished,
    Error,
}

impl HookEvent {
    pub fn is_decision_event(self) -> bool {
        matches!(self, Self::BeforeTool)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct HookInvocation {
    pub event: HookEvent,
    pub cwd: PathBuf,
    pub run_id: String,
    pub session_id: String,
    pub turn: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookOutcome {
    Allow,
    Block { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum HookDecision {
    Allow,
    Block { reason: String },
}

impl From<HookDecision> for HookOutcome {
    fn from(value: HookDecision) -> Self {
        match value {
            HookDecision::Allow => Self::Allow,
            HookDecision::Block { reason } => Self::Block { reason },
        }
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum HookError {
    #[error("hook command failed to spawn: {0}")]
    Spawn(String),
    #[error("hook command timed out after {timeout_ms}ms")]
    Timeout { timeout_ms: u64 },
    #[error("hook command exited with status {status}: {stderr}")]
    Status { status: String, stderr: String },
    #[error("hook command returned invalid JSON: {0}")]
    InvalidJson(String),
}

#[async_trait::async_trait]
pub trait HookExecutor: Send + Sync {
    async fn execute(
        &self,
        handler: &HookHandler,
        invocation: &HookInvocation,
        timeout: Duration,
    ) -> Result<HookOutcome, HookError>;
}

#[derive(Debug, Default)]
pub struct ShellHookExecutor;

#[async_trait::async_trait]
impl HookExecutor for ShellHookExecutor {
    async fn execute(
        &self,
        handler: &HookHandler,
        invocation: &HookInvocation,
        timeout: Duration,
    ) -> Result<HookOutcome, HookError> {
        let input =
            serde_json::to_vec(invocation).map_err(|e| HookError::InvalidJson(e.to_string()))?;

        let mut child = Command::new("sh")
            .arg("-c")
            .arg(&handler.command)
            .current_dir(&invocation.cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| HookError::Spawn(e.to_string()))?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(&input)
                .await
                .map_err(|e| HookError::Spawn(e.to_string()))?;
        }

        let timeout_ms = timeout.as_millis() as u64;
        let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(result) => result.map_err(|e| HookError::Spawn(e.to_string()))?,
            Err(_) => {
                return Err(HookError::Timeout { timeout_ms });
            }
        };

        if !output.status.success() {
            let status = output
                .status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "signal".to_string());
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(HookError::Status { status, stderr });
        }

        if invocation.event.is_decision_event() {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let decision: HookDecision =
                serde_json::from_str(&stdout).map_err(|e| HookError::InvalidJson(e.to_string()))?;
            Ok(decision.into())
        } else {
            Ok(HookOutcome::Allow)
        }
    }
}

#[derive(Clone)]
pub struct HookManager {
    config: Arc<HookConfig>,
    executor: Arc<dyn HookExecutor>,
}

impl HookManager {
    pub fn new(config: HookConfig) -> Option<Self> {
        if !config.enabled_with_handlers() {
            return None;
        }
        Some(Self {
            config: Arc::new(config),
            executor: Arc::new(ShellHookExecutor),
        })
    }

    pub fn with_executor(config: HookConfig, executor: Arc<dyn HookExecutor>) -> Option<Self> {
        if !config.enabled_with_handlers() {
            return None;
        }
        Some(Self {
            config: Arc::new(config),
            executor,
        })
    }

    pub async fn invoke(&self, invocation: HookInvocation) -> HookOutcome {
        let timeout = Duration::from_millis(self.config.timeout_ms);
        for handler in &self.config.handlers {
            if !handler.matches(invocation.event, invocation.tool.as_deref()) {
                continue;
            }
            match self.executor.execute(handler, &invocation, timeout).await {
                Ok(HookOutcome::Allow) => {}
                Ok(block @ HookOutcome::Block { .. }) => return block,
                Err(e) if invocation.event.is_decision_event() => {
                    return HookOutcome::Block {
                        reason: format!("Hook failed: {e}"),
                    };
                }
                Err(e) => {
                    tracing::warn!(
                        event = ?invocation.event,
                        command = %handler.command,
                        error = %e,
                        "hook failed"
                    );
                }
            }
        }

        HookOutcome::Allow
    }
}

#[derive(Debug, Clone)]
pub struct HookRunContext {
    pub run_id: String,
    pub session_id: String,
}
