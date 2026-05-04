//! End-to-end tests for the spill mechanism.
//!
//! Tests the full chain: tool execution → spill to disk → read_file retrieval.

use std::sync::Arc;

use evotengine::spill::FsSpill;
use evotengine::tools::*;
use evotengine::types::*;

use super::ctx;

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Helper: create a FsSpill with a low threshold for testing.
fn test_spill(dir: &std::path::Path) -> Arc<FsSpill> {
    Arc::new(FsSpill::new(dir.to_path_buf()).with_threshold_bytes(100))
}

#[tokio::test]
async fn test_spill_small_result_not_spilled() -> TestResult {
    let tmp_dir = tempfile::tempdir()?;
    let spill = test_spill(tmp_dir.path());

    let req = evotengine::spill::SpillRequest {
        key: "tool_001".into(),
        text: "small output".into(),
    };

    let result = spill.spill(req).await?;
    assert!(result.is_none());
    Ok(())
}

#[tokio::test]
async fn test_spill_large_result_written_to_disk() -> TestResult {
    let tmp_dir = tempfile::tempdir()?;
    let spill = test_spill(tmp_dir.path());

    let large_text = "x".repeat(200);
    let req = evotengine::spill::SpillRequest {
        key: "tool_002".into(),
        text: large_text.clone(),
    };

    let spill_ref = spill
        .spill(req)
        .await?
        .ok_or("expected spill ref for large text")?;
    assert_eq!(spill_ref.size_bytes, 200);
    assert!(spill_ref.path.exists());

    // Verify file content matches
    let on_disk = std::fs::read_to_string(&spill_ref.path)?;
    assert_eq!(on_disk, large_text);
    Ok(())
}

#[tokio::test]
async fn test_spill_preview_truncated() -> TestResult {
    let tmp_dir = tempfile::tempdir()?;
    let spill = Arc::new(
        FsSpill::new(tmp_dir.path().to_path_buf())
            .with_threshold_bytes(100)
            .with_preview_bytes(50),
    );

    let large_text = (1..=100)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");

    let req = evotengine::spill::SpillRequest {
        key: "tool_003".into(),
        text: large_text,
    };

    let result = spill
        .spill(req)
        .await?
        .ok_or("expected spill ref for large text")?;
    assert!(result.preview.len() <= 50);
    Ok(())
}

#[tokio::test]
async fn test_spill_sanitizes_key() -> TestResult {
    let tmp_dir = tempfile::tempdir()?;
    let spill = test_spill(tmp_dir.path());

    let req = evotengine::spill::SpillRequest {
        key: "toolu_01/../../etc/passwd".into(),
        text: "x".repeat(200),
    };

    let result = spill
        .spill(req)
        .await?
        .ok_or("expected spill ref for large text")?;
    // Path should be inside tmp_dir, not escaped
    assert!(result.path.starts_with(tmp_dir.path()));
    // Filename should not contain slashes or dots from traversal
    let filename = result
        .path
        .file_name()
        .ok_or("no filename")?
        .to_str()
        .ok_or("non-utf8 filename")?;
    assert!(!filename.contains('/'));
    assert!(!filename.contains(".."));
    Ok(())
}

#[tokio::test]
async fn test_spill_file_readable_by_read_file_tool() -> TestResult {
    let tmp_dir = tempfile::tempdir()?;
    let spill = test_spill(tmp_dir.path());

    // Generate content large enough to spill
    let lines: Vec<String> = (1..=50).map(|i| format!("output line {i}")).collect();
    let large_text = lines.join("\n");

    let req = evotengine::spill::SpillRequest {
        key: "tool_004".into(),
        text: large_text,
    };

    let spill_ref = spill
        .spill(req)
        .await?
        .ok_or("expected spill ref for large text")?;

    // Now use ReadFileTool to read the spilled file with offset/limit
    let read_tool = ReadFileTool::new();
    let path_str = spill_ref.path.to_str().ok_or("non-utf8 path")?.to_string();
    let result = read_tool
        .execute(
            serde_json::json!({
                "path": path_str,
                "offset": 10,
                "limit": 5,
            }),
            ctx("read_file"),
        )
        .await?;

    let text = match &result.content[0] {
        Content::Text { text } => text,
        _ => return Err("expected text content".into()),
    };
    assert!(text.contains("output line 10"));
    assert!(text.contains("output line 14"));
    assert!(!text.contains("output line 15"));
    Ok(())
}

#[tokio::test]
async fn test_read_file_large_file_without_limit_rejected() -> TestResult {
    let tmp_dir = tempfile::tempdir()?;
    let large_file = tmp_dir.path().join("big.txt");
    // Create a file larger than ReadFileTool's default max_bytes (1MB)
    let content = "x".repeat(1024 * 1024 + 1);
    std::fs::write(&large_file, &content)?;

    let path_str = large_file.to_str().ok_or("non-utf8 path")?.to_string();
    let read_tool = ReadFileTool::new();
    let result = read_tool
        .execute(serde_json::json!({"path": path_str}), ctx("read_file"))
        .await;

    assert!(result.is_err());
    let err = result
        .map_err(|e| e.to_string())
        .err()
        .ok_or("expected error")?;
    assert!(err.contains("too large"));
    Ok(())
}

#[tokio::test]
async fn test_read_file_large_file_with_limit_streams() -> TestResult {
    let tmp_dir = tempfile::tempdir()?;
    let large_file = tmp_dir.path().join("big_stream.txt");
    // Create a file larger than 1MB
    let lines: Vec<String> = (1..=50000).map(|i| format!("data line {i}")).collect();
    let content = lines.join("\n");
    std::fs::write(&large_file, &content)?;

    let path_str = large_file.to_str().ok_or("non-utf8 path")?.to_string();
    let read_tool = ReadFileTool::new();
    let result = read_tool
        .execute(
            serde_json::json!({
                "path": path_str,
                "offset": 100,
                "limit": 10,
            }),
            ctx("read_file"),
        )
        .await?;

    let text = match &result.content[0] {
        Content::Text { text } => text,
        _ => return Err("expected text content".into()),
    };
    assert!(text.contains("data line 100"));
    assert!(text.contains("data line 109"));
    assert!(!text.contains("data line 110"));
    Ok(())
}

#[tokio::test]
async fn test_agent_loop_spill_integration() {
    use evotengine::provider::mock::*;
    use evotengine::*;

    // Create a tool that returns a large result
    struct LargeOutputTool;

    #[async_trait::async_trait]
    impl AgentTool for LargeOutputTool {
        fn name(&self) -> &str {
            "large_tool"
        }
        fn label(&self) -> &str {
            "Large Tool"
        }
        fn description(&self) -> &str {
            "Returns large output"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        async fn execute(
            &self,
            _params: serde_json::Value,
            _ctx: ToolContext,
        ) -> Result<ToolResult, ToolError> {
            let large = "x".repeat(200);
            Ok(ToolResult {
                content: vec![Content::Text { text: large }],
                details: serde_json::Value::Null,
                retention: Retention::Normal,
            })
        }
    }

    let tmp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let spill = Arc::new(FsSpill::new(tmp_dir.path().to_path_buf()).with_threshold_bytes(100));

    // Mock: first response calls the tool, second response is final text
    let responses = vec![
        MockResponse::ToolCalls(vec![MockToolCall {
            name: "large_tool".into(),
            arguments: serde_json::json!({}),
        }]),
        MockResponse::Text("Done".into()),
    ];

    let provider = MockProvider::new(responses);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let cancel = tokio_util::sync::CancellationToken::new();

    let config = AgentLoopConfig {
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
        tool_execution: ToolExecutionStrategy::default(),
        retry_policy: RetryPolicy::disabled(),
        before_turn: None,
        after_turn: None,
        input_filters: vec![],
        spill: Some(spill),
        hook_manager: None,
        hook_context: None,
    };

    let tools: Vec<Box<dyn AgentTool>> = vec![Box::new(LargeOutputTool)];
    let mut context = AgentContext {
        system_prompt: "test".into(),
        messages: vec![],
        tools,
        cwd: std::path::PathBuf::from("/tmp"),
        path_guard: Arc::new(PathGuard::open()),
    };

    let prompts = vec![AgentMessage::Llm(Message::user("run large_tool"))];
    agent_loop(prompts, &mut context, &config, tx, cancel).await;

    // Collect events and find the tool result
    let mut found_spill_message = false;
    while let Ok(event) = rx.try_recv() {
        if let AgentEvent::ToolExecutionEnd { result, .. } = event {
            if let Some(Content::Text { text }) = result.content.first() {
                if text.contains("Tool output was too large") && text.contains("saved to:") {
                    found_spill_message = true;
                    // Verify the referenced file exists
                    if let Some(path_line) = text.lines().find(|l| l.ends_with(".txt")) {
                        let path = std::path::Path::new(path_line.trim());
                        assert!(path.exists());
                        let on_disk =
                            std::fs::read_to_string(path).expect("failed to read spilled file");
                        assert_eq!(on_disk.len(), 200);
                    }
                }
            }
        }
    }
    assert!(found_spill_message, "expected spill message in tool result");

    // Verify context messages contain the spill reference, not the raw 200-byte output
    let has_raw_output = context.messages.iter().any(|m| {
        if let AgentMessage::Llm(Message::ToolResult { content, .. }) = m {
            content.iter().any(|c| match c {
                Content::Text { text } => text == &"x".repeat(200),
                _ => false,
            })
        } else {
            false
        }
    });
    assert!(
        !has_raw_output,
        "context should not contain raw large output after spill"
    );
}
