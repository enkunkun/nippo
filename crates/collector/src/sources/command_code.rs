//! Command Code JSONL セッションファイルのパーサ。
//!
//! `~/.commandcode/projects/{project-dir}/` 以下に保存される JSONL ファイルを読み取り、
//! ユーザーのプロンプト・アシスタントの応答・ツール使用状況を抽出する。
//!
//! Command Code のセッション形式:
//! - 1行目（Line 0）: `{"type":"session", "id":"...", "cwd":"...", "timestamp":"..."}`
//! - 以降の行: `{"type":"message", "id":"...", "timestamp":"...", "message":{"role":"user"|"assistant", "content":[...]}}`
//! - アシスタント行のトークン使用量: `usage: {"inputTokens": N, "outputTokens": M, ...}`
//! - ユーザー行のツール結果（`type: "tool_result"`）はスキップし、純粋なプロンプトテキストのみ抽出する。

use anyhow::{Context, Result};
use rayon::prelude::*;
use serde::Deserialize;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::filter::DateFilter;
use crate::session::{ParsedAssistantEntry, ParsedUserEntry, RawSession};

/// セッションファイルのメタデータ
pub struct SessionFile {
    pub path: PathBuf,
    pub mtime: SystemTime,
}

/// ~/.commandcode/projects/ 以下の全 JSONL ファイルを探索する（*.checkpoints.jsonl は除く）
pub fn discover_session_files(command_code_dir: &Path) -> Result<Vec<SessionFile>> {
    let projects_dir = command_code_dir.join("projects");
    if !projects_dir.exists() {
        anyhow::bail!(
            "Command Code のセッションデータが見つかりません: {}\n\n\
             Command Code を使用すると、セッションデータが自動的にこのディレクトリに保存されます。\n\
             カスタムディレクトリを指定する場合は --command-code-dir オプションを使用してください。",
            projects_dir.display()
        );
    }

    let pattern = format!("{}/**/*.jsonl", projects_dir.display());
    let mut files = Vec::new();

    for entry in glob::glob(&pattern).context("Failed to read glob pattern")? {
        let path = match entry {
            Ok(p) => p,
            Err(_) => continue,
        };
        // チェックポイントファイルは除外
        if path.to_string_lossy().ends_with(".checkpoints.jsonl") {
            continue;
        }
        let metadata = match fs::metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !metadata.is_file() {
            continue;
        }
        let mtime = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        files.push(SessionFile { path, mtime });
    }

    Ok(files)
}

// ---------------------------------------------------------------------------
// JSONL データ構造
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(tag = "type")]
enum JournalEntry {
    #[serde(rename = "session")]
    Session(SessionHeader),
    #[serde(rename = "message")]
    Message(MessageEntry),
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct SessionHeader {
    id: Option<String>,
    cwd: Option<String>,
}

#[derive(Deserialize)]
struct MessageEntry {
    timestamp: Option<String>,
    message: Option<MessagePayload>,
    usage: Option<TokenUsage>,
}

#[derive(Deserialize)]
struct MessagePayload {
    role: Option<String>,
    content: Option<Vec<ContentBlock>>,
    usage: Option<TokenUsage>,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        name: Option<String>,
        input: Option<serde_json::Value>,
    },
    #[serde(other)]
    Other,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TokenUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

const MAX_PROMPT_LEN: usize = 500;

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max).collect();
        format!("{truncated}...")
    }
}

fn extract_user_text(blocks: &[ContentBlock]) -> Option<String> {
    let texts: Vec<&str> = blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    if texts.is_empty() {
        None
    } else {
        let joined = texts.join("\n");
        let trimmed = joined.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(truncate(trimmed, MAX_PROMPT_LEN))
        }
    }
}

fn extract_tool_info(blocks: &[ContentBlock]) -> (Vec<String>, Vec<String>) {
    let mut tool_names = Vec::new();
    let mut file_paths = Vec::new();

    for block in blocks {
        if let ContentBlock::ToolUse {
            name: Some(n),
            input,
        } = block
        {
            tool_names.push(n.clone());

            if let Some(input_val) = input {
                // read_file, write_file, edit_file (file_path)
                if let Some(fp) = input_val.get("file_path").and_then(|v| v.as_str()) {
                    file_paths.push(fp.to_string());
                }
                // read_directory, glob, grep (path)
                if let Some(fp) = input_val.get("path").and_then(|v| v.as_str()) {
                    file_paths.push(fp.to_string());
                }
                // read_file paths array
                if let Some(paths) = input_val.get("paths").and_then(|v| v.as_array()) {
                    for p in paths {
                        if let Some(s) = p.as_str() {
                            file_paths.push(s.to_string());
                        }
                    }
                }
            }
        }
    }

    (tool_names, file_paths)
}

fn extract_project_from_cwd(cwd: &str) -> String {
    Path::new(cwd)
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| cwd.to_string())
}

// ---------------------------------------------------------------------------
// 1ファイルのパース
// ---------------------------------------------------------------------------

pub fn parse_session_file(path: &Path, filter: &DateFilter) -> Result<Option<RawSession>> {
    let file = File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    let reader = BufReader::new(file);

    let mut user_entries = Vec::new();
    let mut assistant_entries = Vec::new();
    let mut session_id = String::new();
    let mut project = String::new();
    let mut project_path = String::new();

    for line_res in reader.lines() {
        let line = match line_res {
            Ok(l) => l,
            Err(_) => continue,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let entry: JournalEntry = match serde_json::from_str(trimmed) {
            Ok(e) => e,
            Err(_) => continue,
        };

        match entry {
            JournalEntry::Session(session) => {
                if session_id.is_empty()
                    && let Some(id) = session.id
                {
                    session_id = id;
                }
                if project.is_empty()
                    && let Some(cwd) = session.cwd
                {
                    project = extract_project_from_cwd(&cwd);
                    project_path = cwd;
                }
            }
            JournalEntry::Message(msg_entry) => {
                let Some(ts) = msg_entry.timestamp else {
                    continue;
                };

                if !filter.matches(&ts) {
                    continue;
                }

                let Some(payload) = msg_entry.message else {
                    continue;
                };

                let role = payload.role.as_deref().unwrap_or("");
                let blocks = payload.content.as_deref().unwrap_or(&[]);

                match role {
                    "user" => {
                        if let Some(text) = extract_user_text(blocks) {
                            user_entries.push(ParsedUserEntry {
                                timestamp: ts,
                                text,
                            });
                        }
                    }
                    "assistant" => {
                        let (tool_uses, file_paths) = extract_tool_info(blocks);
                        let (input_tokens, output_tokens) = msg_entry
                            .usage
                            .as_ref()
                            .or(payload.usage.as_ref())
                            .map(|u| (u.input_tokens.unwrap_or(0), u.output_tokens.unwrap_or(0)))
                            .unwrap_or((0, 0));

                        assistant_entries.push(ParsedAssistantEntry {
                            timestamp: ts,
                            message_count: 1,
                            tool_uses,
                            input_tokens,
                            output_tokens,
                            file_paths,
                        });
                    }
                    _ => {}
                }
            }
            JournalEntry::Other => {}
        }
    }

    if user_entries.is_empty() && assistant_entries.is_empty() {
        return Ok(None);
    }

    if session_id.is_empty() {
        session_id = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
    }

    Ok(Some(RawSession {
        session_id,
        project,
        project_path,
        git_branch: None,
        user_entries,
        assistant_entries,
    }))
}

// ---------------------------------------------------------------------------
// 並列収集
// ---------------------------------------------------------------------------

pub fn collect_sessions(command_code_dir: &Path, filter: &DateFilter) -> Result<Vec<RawSession>> {
    let files = discover_session_files(command_code_dir)?;

    let cutoff = filter.mtime_cutoff();
    let candidates: Vec<&SessionFile> = files
        .iter()
        .filter(|f| cutoff.map(|c| f.mtime >= c).unwrap_or(true))
        .collect();

    let sessions: Vec<RawSession> = candidates
        .par_iter()
        .filter_map(|sf| parse_session_file(&sf.path, filter).ok().flatten())
        .collect();

    Ok(sessions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn sample_session_content(
        session_id: &str,
        cwd: &str,
        user_prompt: &str,
        tool_name: &str,
        file_path: &str,
    ) -> String {
        format!(
            "{{\"type\":\"session\",\"version\":3,\"id\":\"{session_id}\",\"timestamp\":\"2026-10-14T01:00:00.000Z\",\"cwd\":\"{cwd}\"}}\n\
             {{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2026-10-14T01:00:10.000Z\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"{user_prompt}\"}}]}}}}\n\
             {{\"type\":\"message\",\"id\":\"m2\",\"parentId\":\"m1\",\"timestamp\":\"2026-10-14T01:00:15.000Z\",\"usage\":{{\"inputTokens\":500,\"outputTokens\":42}},\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"thinking\",\"thinking\":\"hmm\"}},{{\"type\":\"text\",\"text\":\"checking\"}},{{\"type\":\"tool_use\",\"name\":\"{tool_name}\",\"input\":{{\"file_path\":\"{file_path}\"}}}}]}}}}\n\
             {{\"type\":\"message\",\"id\":\"m3\",\"parentId\":\"m2\",\"timestamp\":\"2026-10-14T01:00:20.000Z\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"tool_result\",\"tool_use_id\":\"call1\",\"content\":[{{\"type\":\"text\",\"text\":\"done\"}}]}}]}}}}\n"
        )
    }

    #[test]
    fn parses_command_code_session_file() {
        let dir = tempdir().expect("tempdir");
        let proj_dir = dir.path().join("projects").join("home-user-git-myapp");
        std::fs::create_dir_all(&proj_dir).expect("create dir");

        let file_path = proj_dir.join("test-sess.jsonl");
        let content = sample_session_content(
            "sess-123",
            "/home/user/git/myapp",
            "Suicaログインを実装して",
            "read_file",
            "/home/user/git/myapp/src/suica.ts",
        );
        std::fs::write(&file_path, content).expect("write session");

        let filter = DateFilter::from_days(0); // all time
        let session = parse_session_file(&file_path, &filter)
            .expect("parse")
            .expect("session");

        assert_eq!(session.session_id, "sess-123");
        assert_eq!(session.project, "myapp");
        assert_eq!(session.project_path, "/home/user/git/myapp");
        assert_eq!(session.user_entries.len(), 1);
        assert_eq!(session.user_entries[0].text, "Suicaログインを実装して");
        assert_eq!(session.assistant_entries.len(), 1);
        assert_eq!(session.assistant_entries[0].tool_uses, vec!["read_file"]);
        assert_eq!(
            session.assistant_entries[0].file_paths,
            vec!["/home/user/git/myapp/src/suica.ts"]
        );
        assert_eq!(session.assistant_entries[0].input_tokens, 500);
        assert_eq!(session.assistant_entries[0].output_tokens, 42);
    }

    #[test]
    fn ignores_checkpoints_files() {
        let dir = tempdir().expect("tempdir");
        let proj_dir = dir.path().join("projects").join("home-user-git-myapp");
        std::fs::create_dir_all(&proj_dir).expect("create dir");

        std::fs::write(proj_dir.join("sess-1.jsonl"), b"").expect("write sess");
        std::fs::write(proj_dir.join("sess-1.checkpoints.jsonl"), b"").expect("write cp");

        let files = discover_session_files(dir.path()).expect("discover");
        assert_eq!(files.len(), 1);
        assert!(files[0].path.ends_with("sess-1.jsonl"));
    }
}
