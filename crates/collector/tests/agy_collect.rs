use anyhow::{Context, Result};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::path::Path;
use std::process::Command;
use tempfile::{TempDir, tempdir};

const PROMPT: &str = "Fix the authentication error in api client";

fn setup_agy_fixture(
    conversation_id: &str,
    workspace_path: &str,
    last_modified: &str,
    transcript_records: &[Value],
) -> Result<TempDir> {
    let dir = tempdir().context("create tempdir")?;
    let db_path = dir.path().join("conversation_summaries.db");
    let conn = Connection::open(&db_path)?;
    conn.execute(
        "CREATE TABLE conversation_summaries (
            conversation_id TEXT PRIMARY KEY,
            workspace_uris TEXT,
            last_modified_time TEXT
        );",
        [],
    )?;
    let uris_json = json!([format!("file://{workspace_path}")]).to_string();
    conn.execute(
        "INSERT INTO conversation_summaries VALUES (?1, ?2, ?3)",
        (conversation_id, uris_json, last_modified),
    )?;

    let logs_dir = dir
        .path()
        .join("brain")
        .join(conversation_id)
        .join(".system_generated")
        .join("logs");
    std::fs::create_dir_all(&logs_dir)?;

    let mut transcript_text = String::new();
    for rec in transcript_records {
        transcript_text.push_str(&rec.to_string());
        transcript_text.push('\n');
    }
    std::fs::write(logs_dir.join("transcript.jsonl"), transcript_text)?;

    Ok(dir)
}

fn collect_agy(
    agy_dir: &Path,
    source: &str,
    from: &str,
    to: &str,
    extra_args: &[&str],
    current_conv_id: Option<&str>,
) -> Result<Value> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nippo"));
    cmd.args(["collect", "--source", source, "--from", from, "--to", to])
        .arg("--agy-dir")
        .arg(agy_dir)
        .arg("--claude-dir")
        .arg(agy_dir.join("absent-claude"))
        .arg("--codex-dir")
        .arg(agy_dir.join("absent-codex"))
        .arg("--opencode-dir")
        .arg(agy_dir.join("absent-opencode"))
        .arg("--command-code-dir")
        .arg(agy_dir.join("absent-command-code"))
        .args(extra_args)
        .env("TZ", "Asia/Tokyo")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("COMMANDCODE_SESSION_ID");

    if let Some(cid) = current_conv_id {
        cmd.env("ANTIGRAVITY_CONVERSATION_ID", cid);
    } else {
        cmd.env_remove("ANTIGRAVITY_CONVERSATION_ID");
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
fn collects_agy_session_with_metadata_and_prompts() -> Result<()> {
    let records = vec![
        json!({
            "step_index": 0,
            "source": "USER_EXPLICIT",
            "type": "USER_INPUT",
            "created_at": "2026-10-11T12:00:00Z",
            "content": format!("<USER_REQUEST>\n{PROMPT}\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\ntime\n</ADDITIONAL_METADATA>")
        }),
        json!({
            "step_index": 1,
            "source": "MODEL",
            "type": "PLANNER_RESPONSE",
            "created_at": "2026-10-11T12:00:05Z",
            "input_tokens": 1200,
            "output_tokens": 80,
            "tool_calls": [
                {
                    "name": "view_file",
                    "args": {
                        "AbsolutePath": "\"/Users/tendo/git/myapp/src/lib.rs\""
                    }
                }
            ]
        }),
    ];

    let dir = setup_agy_fixture(
        "conv-123",
        "/Users/tendo/git/myapp",
        "2026-10-11 12:00:05.000000+00:00",
        &records,
    )?;

    let output = collect_agy(dir.path(), "agy", "2026-10-11", "2026-10-11", &[], None)?;

    assert_eq!(output["meta"]["total_sessions"], 1);
    assert_eq!(output["stats"]["total_user_messages"], 1);
    assert_eq!(output["stats"]["total_assistant_messages"], 1);
    assert_eq!(output["stats"]["total_input_tokens"], 1200);
    assert_eq!(output["stats"]["total_output_tokens"], 80);
    assert_eq!(output["sessions"][0]["session_id"], "conv-123");
    assert_eq!(output["sessions"][0]["project"], "myapp");
    assert_eq!(
        output["sessions"][0]["project_path"],
        "/Users/tendo/git/myapp"
    );
    assert_eq!(output["sessions"][0]["user_prompts"][0]["text"], PROMPT);
    assert_eq!(
        output["sessions"][0]["files_touched"],
        json!(["src/lib.rs"])
    );

    Ok(())
}

#[test]
fn excludes_current_session_by_default_and_includes_with_flag() -> Result<()> {
    let records = vec![
        json!({
            "step_index": 0,
            "source": "USER_EXPLICIT",
            "type": "USER_INPUT",
            "created_at": "2026-10-11T12:00:00Z",
            "content": format!("<USER_REQUEST>\n{PROMPT}\n</USER_REQUEST>")
        }),
        json!({
            "step_index": 1,
            "source": "MODEL",
            "type": "PLANNER_RESPONSE",
            "created_at": "2026-10-11T12:00:05Z",
            "input_tokens": 100,
            "output_tokens": 20,
            "tool_calls": []
        }),
    ];

    let dir = setup_agy_fixture(
        "current-conv",
        "/Users/tendo/git/myapp",
        "2026-10-11 12:00:05.000000+00:00",
        &records,
    )?;

    // By default, current-conv is excluded
    let output_excluded = collect_agy(
        dir.path(),
        "agy",
        "2026-10-11",
        "2026-10-11",
        &[],
        Some("current-conv"),
    )?;
    assert_eq!(output_excluded["meta"]["total_sessions"], 0);

    // With --include-self, it is included
    let output_included = collect_agy(
        dir.path(),
        "agy",
        "2026-10-11",
        "2026-10-11",
        &["--include-self"],
        Some("current-conv"),
    )?;
    assert_eq!(output_included["meta"]["total_sessions"], 1);
    assert_eq!(output_included["sessions"][0]["session_id"], "current-conv");

    Ok(())
}

#[test]
fn auto_source_detects_agy_when_conversation_id_is_set() -> Result<()> {
    let records = vec![
        json!({
            "step_index": 0,
            "source": "USER_EXPLICIT",
            "type": "USER_INPUT",
            "created_at": "2026-10-11T12:00:00Z",
            "content": format!("<USER_REQUEST>\n{PROMPT}\n</USER_REQUEST>")
        }),
        json!({
            "step_index": 1,
            "source": "MODEL",
            "type": "PLANNER_RESPONSE",
            "created_at": "2026-10-11T12:00:05Z",
            "input_tokens": 10,
            "output_tokens": 5,
            "tool_calls": []
        }),
    ];

    let dir = setup_agy_fixture(
        "other-conv",
        "/Users/tendo/git/myapp",
        "2026-10-11 12:00:05.000000+00:00",
        &records,
    )?;

    let output = collect_agy(
        dir.path(),
        "auto",
        "2026-10-11",
        "2026-10-11",
        &[],
        Some("my-active-conv"),
    )?;

    assert_eq!(output["meta"]["source"]["resolved"], json!(["agy"]));
    assert_eq!(output["meta"]["total_sessions"], 1);
    assert_eq!(output["sessions"][0]["session_id"], "other-conv");

    Ok(())
}
