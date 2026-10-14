use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::{TempDir, tempdir};

const PROMPT: &str = "Fix the authentication bug in api client";

fn setup_command_code_fixture(
    project_slug: &str,
    session_id: &str,
    records: &[Value],
) -> Result<TempDir> {
    let dir = tempdir().context("create tempdir")?;
    let project_dir = dir.path().join("projects").join(project_slug);
    fs::create_dir_all(&project_dir)?;

    let mut lines = Vec::new();
    for rec in records {
        lines.push(rec.to_string());
    }
    fs::write(
        project_dir.join(format!("{session_id}.jsonl")),
        lines.join("\n"),
    )?;

    Ok(dir)
}

fn collect_command_code(
    cc_dir: &Path,
    source: &str,
    from: &str,
    to: &str,
    extra_args: &[&str],
    current_session_id: Option<&str>,
) -> Result<Value> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nippo"));
    cmd.args(["collect", "--source", source, "--from", from, "--to", to])
        .arg("--command-code-dir")
        .arg(cc_dir)
        .arg("--claude-dir")
        .arg(cc_dir.join("absent-claude"))
        .arg("--codex-dir")
        .arg(cc_dir.join("absent-codex"))
        .arg("--opencode-dir")
        .arg(cc_dir.join("absent-opencode"))
        .arg("--agy-dir")
        .arg(cc_dir.join("absent-agy"))
        .args(extra_args)
        .env("TZ", "Asia/Tokyo")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("ANTIGRAVITY_CONVERSATION_ID");

    if let Some(sid) = current_session_id {
        cmd.env("COMMANDCODE_SESSION_ID", sid);
    } else {
        cmd.env_remove("COMMANDCODE_SESSION_ID");
    }

    let output = cmd.output()?;
    assert!(
        output.status.success(),
        "collect failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).context("parse collect JSON")
}

#[test]
fn collects_command_code_session_with_metadata_and_prompts() -> Result<()> {
    let records = vec![
        json!({
            "type": "session",
            "version": 3,
            "id": "cc-session-123",
            "cwd": "/home/user/git/myapp",
            "timestamp": "2026-10-12T10:00:00Z"
        }),
        json!({
            "type": "message",
            "id": "msg-1",
            "timestamp": "2026-10-12T10:00:01Z",
            "message": {
                "role": "user",
                "content": [
                    {
                        "type": "text",
                        "text": PROMPT
                    }
                ]
            }
        }),
        json!({
            "type": "message",
            "id": "msg-2",
            "timestamp": "2026-10-12T10:00:05Z",
            "message": {
                "role": "assistant",
                "content": [
                    {
                        "type": "text",
                        "text": "I will fix it"
                    },
                    {
                        "type": "tool_use",
                        "id": "call-1",
                        "name": "Edit",
                        "input": {
                            "file_path": "/home/user/git/myapp/src/auth.rs"
                        }
                    }
                ],
                "usage": {
                    "inputTokens": 500,
                    "outputTokens": 50
                }
            }
        }),
    ];

    let dir = setup_command_code_fixture("home-user-git-myapp", "cc-session-123", &records)?;

    let output = collect_command_code(
        dir.path(),
        "command-code",
        "2026-10-12",
        "2026-10-12",
        &[],
        None,
    )?;

    assert_eq!(output["meta"]["total_sessions"], 1);
    assert_eq!(output["stats"]["total_user_messages"], 1);
    assert_eq!(output["stats"]["total_assistant_messages"], 1);
    assert_eq!(output["stats"]["total_input_tokens"], 500);
    assert_eq!(output["stats"]["total_output_tokens"], 50);
    assert_eq!(output["sessions"][0]["session_id"], "cc-session-123");
    assert_eq!(output["sessions"][0]["project"], "myapp");
    assert_eq!(
        output["sessions"][0]["project_path"],
        "/home/user/git/myapp"
    );
    assert_eq!(output["sessions"][0]["user_prompts"][0]["text"], PROMPT);
    assert_eq!(
        output["sessions"][0]["files_touched"],
        json!(["src/auth.rs"])
    );

    Ok(())
}

#[test]
fn excludes_current_session_by_default_and_includes_with_flag() -> Result<()> {
    let records = vec![
        json!({
            "type": "session",
            "version": 3,
            "id": "current-cc-session",
            "cwd": "/home/user/git/myapp",
            "timestamp": "2026-10-12T10:00:00Z"
        }),
        json!({
            "type": "message",
            "id": "msg-1",
            "timestamp": "2026-10-12T10:00:01Z",
            "message": {
                "role": "user",
                "content": [
                    {
                        "type": "text",
                        "text": PROMPT
                    }
                ]
            }
        }),
        json!({
            "type": "message",
            "id": "msg-2",
            "timestamp": "2026-10-12T10:00:05Z",
            "message": {
                "role": "assistant",
                "content": [
                    {
                        "type": "text",
                        "text": "Done"
                    }
                ],
                "usage": {
                    "inputTokens": 100,
                    "outputTokens": 20
                }
            }
        }),
    ];

    let dir = setup_command_code_fixture("home-user-git-myapp", "current-cc-session", &records)?;

    // By default, current-cc-session is excluded
    let output_excluded = collect_command_code(
        dir.path(),
        "command-code",
        "2026-10-12",
        "2026-10-12",
        &[],
        Some("current-cc-session"),
    )?;
    assert_eq!(output_excluded["meta"]["total_sessions"], 0);

    // With --include-self, it is included
    let output_included = collect_command_code(
        dir.path(),
        "command-code",
        "2026-10-12",
        "2026-10-12",
        &["--include-self"],
        Some("current-cc-session"),
    )?;
    assert_eq!(output_included["meta"]["total_sessions"], 1);
    assert_eq!(
        output_included["sessions"][0]["session_id"],
        "current-cc-session"
    );

    Ok(())
}

#[test]
fn auto_source_detects_command_code_when_session_id_is_set() -> Result<()> {
    let records = vec![
        json!({
            "type": "session",
            "version": 3,
            "id": "other-cc-session",
            "cwd": "/home/user/git/myapp",
            "timestamp": "2026-10-12T10:00:00Z"
        }),
        json!({
            "type": "message",
            "id": "msg-1",
            "timestamp": "2026-10-12T10:00:01Z",
            "message": {
                "role": "user",
                "content": [
                    {
                        "type": "text",
                        "text": PROMPT
                    }
                ]
            }
        }),
        json!({
            "type": "message",
            "id": "msg-2",
            "timestamp": "2026-10-12T10:00:05Z",
            "message": {
                "role": "assistant",
                "content": [
                    {
                        "type": "text",
                        "text": "Done"
                    }
                ],
                "usage": {
                    "inputTokens": 10,
                    "outputTokens": 5
                }
            }
        }),
    ];

    let dir = setup_command_code_fixture("home-user-git-myapp", "other-cc-session", &records)?;

    let output = collect_command_code(
        dir.path(),
        "auto",
        "2026-10-12",
        "2026-10-12",
        &[],
        Some("active-cc-session"),
    )?;

    assert_eq!(
        output["meta"]["source"]["resolved"],
        json!(["command-code"])
    );
    assert_eq!(output["meta"]["total_sessions"], 1);
    assert_eq!(output["sessions"][0]["session_id"], "other-cc-session");

    Ok(())
}
