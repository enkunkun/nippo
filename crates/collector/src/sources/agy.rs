//! agy (Antigravity CLI) 履歴のコレクター。
//!
//! `~/.gemini/antigravity-cli/conversation_summaries.db` の SQLite
//! (`conversation_summaries` テーブル) からセッション一覧とワークスペースを取得し、
//! `~/.gemini/antigravity-cli/brain/{conversation_id}/.system_generated/logs/transcript.jsonl`
//! からユーザープロンプト、ツール呼び出し、トークン使用量を抽出する。

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::Connection;
use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::filter::DateFilter;
use crate::session::{ParsedAssistantEntry, ParsedUserEntry, RawSession};

const MAX_PROMPT_LEN: usize = 500;

pub fn discover_history_files(agy_dir: &Path) -> Result<Vec<PathBuf>> {
    let db_path = agy_dir.join("conversation_summaries.db");
    if !db_path.exists() {
        anyhow::bail!(
            "agy の履歴データが見つかりません: {}\n\n\
             conversation_summaries.db が必要です。\n\
             カスタムディレクトリを指定する場合は --agy-dir オプションを使用してください。",
            agy_dir.display()
        );
    }
    Ok(vec![db_path])
}

pub fn collect_sessions(agy_dir: &Path, filter: &DateFilter) -> Result<Vec<RawSession>> {
    let db_path = agy_dir.join("conversation_summaries.db");
    if !db_path.exists() {
        return Ok(Vec::new());
    }

    let conn = Connection::open(&db_path)
        .with_context(|| format!("Failed to open {}", db_path.display()))?;

    let mut stmt = conn
        .prepare(
            "SELECT conversation_id, workspace_uris, last_modified_time \
             FROM conversation_summaries",
        )
        .context("Failed to prepare conversation_summaries query")?;

    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .context("Failed to read conversation_summaries")?;

    let mut sessions = Vec::new();
    let brain_dir = agy_dir.join("brain");

    for row in rows {
        let (conversation_id, workspace_uris_json, last_modified_str) =
            row.context("Failed to decode conversation_summaries row")?;

        // Pre-filter: if last_modified_time is strictly before filter start date, skip reading transcript
        if let Some(cutoff) = filter.mtime_cutoff()
            && let Some(last_mod) = parse_sqlite_timestamp(&last_modified_str)
            && SystemTime::from(last_mod) < cutoff
        {
            continue;
        }

        let transcript_path = brain_dir
            .join(&conversation_id)
            .join(".system_generated")
            .join("logs")
            .join("transcript.jsonl");

        if !transcript_path.exists() {
            continue;
        }

        let (user_entries, assistant_entries, detected_cwd) =
            parse_transcript(&transcript_path, filter)?;

        if user_entries.is_empty() && assistant_entries.is_empty() {
            continue;
        }

        let (project_path, project) =
            resolve_project(&workspace_uris_json, detected_cwd.as_deref());

        sessions.push(RawSession {
            session_id: conversation_id,
            project,
            project_path,
            git_branch: None,
            user_entries,
            assistant_entries,
        });
    }

    Ok(sessions)
}

fn parse_sqlite_timestamp(s: &str) -> Option<DateTime<Utc>> {
    let s_t = s.replace(' ', "T");
    if let Ok(dt) = DateTime::parse_from_rfc3339(&s_t) {
        return Some(dt.with_timezone(&Utc));
    }
    None
}

fn resolve_project(workspace_uris_json: &str, fallback_cwd: Option<&str>) -> (String, String) {
    if let Ok(Value::Array(arr)) = serde_json::from_str(workspace_uris_json) {
        for item in arr {
            if let Some(uri) = item.as_str()
                && let Some(path) = uri_to_path(uri)
                && !path.is_empty()
            {
                let project = extract_project_name(&path);
                return (path, project);
            }
        }
    }
    if let Some(cwd) = fallback_cwd
        && !cwd.is_empty()
    {
        let project = extract_project_name(cwd);
        return (cwd.to_string(), project);
    }
    ("".to_string(), "unknown".to_string())
}

fn uri_to_path(uri: &str) -> Option<String> {
    if let Some(stripped) = uri.strip_prefix("file://") {
        Some(stripped.to_string())
    } else if uri.starts_with('/') {
        Some(uri.to_string())
    } else {
        None
    }
}

fn extract_project_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("unknown")
        .to_string()
}

fn parse_transcript(
    path: &Path,
    filter: &DateFilter,
) -> Result<(
    Vec<ParsedUserEntry>,
    Vec<ParsedAssistantEntry>,
    Option<String>,
)> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);

    let mut user_entries = Vec::new();
    let mut assistant_entries = Vec::new();
    let mut detected_cwd: Option<String> = None;

    for line_result in reader.lines() {
        let line = match line_result {
            Ok(l) => l,
            Err(_) => continue,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let val: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let step_type = val.get("type").and_then(Value::as_str).unwrap_or("");
        let created_at = val.get("created_at").and_then(Value::as_str).unwrap_or("");

        match step_type {
            "USER_INPUT" => {
                if !filter.matches(created_at) {
                    continue;
                }
                let raw_content = val.get("content").and_then(Value::as_str).unwrap_or("");
                let prompt = extract_user_prompt(raw_content);
                if !prompt.is_empty() {
                    user_entries.push(ParsedUserEntry {
                        timestamp: created_at.to_string(),
                        text: truncate(&prompt, MAX_PROMPT_LEN),
                    });
                }
            }
            "PLANNER_RESPONSE" => {
                let (tools, files, cwd) = extract_tool_info(val.get("tool_calls"));
                if detected_cwd.is_none() && cwd.is_some() {
                    detected_cwd = cwd;
                }

                if !filter.matches(created_at) {
                    continue;
                }

                let input_tokens = val.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
                let output_tokens = val
                    .get("output_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);

                assistant_entries.push(ParsedAssistantEntry {
                    timestamp: created_at.to_string(),
                    message_count: 1,
                    tool_uses: tools,
                    input_tokens,
                    output_tokens,
                    file_paths: files,
                });
            }
            _ => {}
        }
    }

    Ok((user_entries, assistant_entries, detected_cwd))
}

fn extract_user_prompt(content: &str) -> String {
    if let Some(start) = content.find("<USER_REQUEST>") {
        let after_start = &content[start + "<USER_REQUEST>".len()..];
        if let Some(end) = after_start.find("</USER_REQUEST>") {
            return after_start[..end].trim().to_string();
        }
    }
    if let Some(meta_start) = content.find("<ADDITIONAL_METADATA>") {
        return content[..meta_start].trim().to_string();
    }
    content.trim().to_string()
}

fn extract_tool_info(tool_calls: Option<&Value>) -> (Vec<String>, Vec<String>, Option<String>) {
    let mut tool_names = Vec::new();
    let mut file_paths = Vec::new();
    let mut cwd: Option<String> = None;

    let Some(arr) = tool_calls.and_then(Value::as_array) else {
        return (tool_names, file_paths, cwd);
    };

    for tc in arr {
        if let Some(name) = tc.get("name").and_then(Value::as_str) {
            tool_names.push(name.to_string());
        }

        let args_val = tc.get("args");
        let parsed_args_holder;
        let args_obj = match args_val {
            Some(Value::Object(map)) => Some(map),
            Some(Value::String(s)) => {
                if let Ok(Value::Object(map)) = serde_json::from_str(s) {
                    parsed_args_holder = map;
                    Some(&parsed_args_holder)
                } else {
                    None
                }
            }
            _ => None,
        };

        if let Some(args) = args_obj {
            for key in &[
                "AbsolutePath",
                "TargetFile",
                "DirectoryPath",
                "SearchPath",
                "file_path",
                "filePath",
                "path",
            ] {
                if let Some(val_str) = args.get(*key).and_then(Value::as_str) {
                    let cleaned = clean_str(val_str);
                    if !cleaned.is_empty() {
                        file_paths.push(cleaned.to_string());
                    }
                }
            }

            if cwd.is_none()
                && let Some(val_str) = args.get("Cwd").and_then(Value::as_str)
            {
                let cleaned = clean_str(val_str);
                if !cleaned.is_empty() {
                    cwd = Some(cleaned.to_string());
                }
            }
        }
    }

    (tool_names, file_paths, cwd)
}

fn clean_str(s: &str) -> &str {
    s.trim().trim_matches('"').trim()
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max).collect();
        format!("{truncated}...")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn setup_test_agy_env(
        dir: &Path,
        conversation_id: &str,
        workspace_uris: &str,
        last_modified: &str,
        transcript_content: &str,
    ) {
        let db_path = dir.join("conversation_summaries.db");
        let conn = Connection::open(&db_path).expect("open db");
        conn.execute(
            "CREATE TABLE IF NOT EXISTS conversation_summaries (
                conversation_id TEXT PRIMARY KEY,
                workspace_uris TEXT,
                last_modified_time TEXT
            )",
            [],
        )
        .expect("create table");

        conn.execute(
            "INSERT INTO conversation_summaries (conversation_id, workspace_uris, last_modified_time)
             VALUES (?1, ?2, ?3)",
            (conversation_id, workspace_uris, last_modified),
        )
        .expect("insert summary");

        let log_dir = dir
            .join("brain")
            .join(conversation_id)
            .join(".system_generated")
            .join("logs");
        std::fs::create_dir_all(&log_dir).expect("create log dir");
        std::fs::write(log_dir.join("transcript.jsonl"), transcript_content)
            .expect("write transcript");
    }

    #[test]
    fn discover_history_files_checks_existence() {
        let dir = tempdir().expect("tempdir");
        assert!(discover_history_files(dir.path()).is_err());

        std::fs::write(dir.path().join("conversation_summaries.db"), b"").expect("write db");
        let files = discover_history_files(dir.path()).expect("discover");
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("conversation_summaries.db"));
    }

    #[test]
    fn collects_agy_session_with_user_and_assistant() {
        let dir = tempdir().expect("tempdir");
        let cid = "test-conv-1";
        let transcript = r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","created_at":"2026-10-11T12:00:00Z","content":"<USER_REQUEST>\nテストプロンプト\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\ntime\n</ADDITIONAL_METADATA>"}
{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","created_at":"2026-10-11T12:00:05Z","input_tokens":100,"output_tokens":20,"tool_calls":[{"name":"view_file","args":{"AbsolutePath":"\"/Users/tendo/git/nippo/README.md\""}}]}
"#;
        setup_test_agy_env(
            dir.path(),
            cid,
            r#"["file:///Users/tendo/git/nippo"]"#,
            "2026-10-11 12:00:05.000000+00:00",
            transcript,
        );

        let filter = DateFilter::from_days(0); // all time
        let sessions = collect_sessions(dir.path(), &filter).expect("collect");
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.session_id, cid);
        assert_eq!(s.project, "nippo");
        assert_eq!(s.project_path, "/Users/tendo/git/nippo");
        assert_eq!(s.user_entries.len(), 1);
        assert_eq!(s.user_entries[0].text, "テストプロンプト");
        assert_eq!(s.assistant_entries.len(), 1);
        assert_eq!(s.assistant_entries[0].tool_uses, vec!["view_file"]);
        assert_eq!(
            s.assistant_entries[0].file_paths,
            vec!["/Users/tendo/git/nippo/README.md"]
        );
        assert_eq!(s.assistant_entries[0].input_tokens, 100);
        assert_eq!(s.assistant_entries[0].output_tokens, 20);
    }

    #[test]
    fn extracts_user_prompt_cleanly() {
        assert_eq!(
            extract_user_prompt(
                "<USER_REQUEST>\nhello world\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nfoo\n</ADDITIONAL_METADATA>"
            ),
            "hello world"
        );
        assert_eq!(
            extract_user_prompt("plain text prompt<ADDITIONAL_METADATA>\nbar"),
            "plain text prompt"
        );
        assert_eq!(extract_user_prompt("simple prompt"), "simple prompt");
    }

    #[test]
    fn resolves_project_from_workspace_and_fallback_cwd() {
        let (path, proj) = resolve_project(r#"["file:///Users/tendo/git/compose"]"#, None);
        assert_eq!(path, "/Users/tendo/git/compose");
        assert_eq!(proj, "compose");

        let (path2, proj2) = resolve_project("[]", Some("/Users/tendo/git/articles"));
        assert_eq!(path2, "/Users/tendo/git/articles");
        assert_eq!(proj2, "articles");
    }
}
