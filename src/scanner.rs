use crate::model::{ScannedThread, SessionMessage, ThreadRecord};
use crate::sqlite_adapter;
use anyhow::{anyhow, Context, Result};
use rusqlite::{Connection, OptionalExtension};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;
use walkdir::WalkDir;

#[derive(Debug, Default)]
struct DbMetadata {
    title: String,
    created_at: i64,
    updated_at: i64,
    cwd: String,
    source: String,
    thread_source: Option<String>,
    model_provider: String,
    cli_version: String,
    first_user_message: String,
    sandbox_policy: Option<String>,
    approval_mode: Option<String>,
    model: Option<String>,
    reasoning_effort: Option<String>,
}

pub fn scan_codex_home(home: &Path, state_db: Option<&Path>) -> Result<Vec<ScannedThread>> {
    let db_metadata = state_db
        .map(load_db_metadata)
        .transpose()?
        .unwrap_or_default();
    let mut result = Vec::new();
    scan_directory(&home.join("sessions"), false, &db_metadata, &mut result)?;
    scan_directory(
        &home.join("archived_sessions"),
        true,
        &db_metadata,
        &mut result,
    )?;
    result.sort_by(|left, right| {
        left.record
            .updated_at
            .cmp(&right.record.updated_at)
            .then_with(|| left.record.id.cmp(&right.record.id))
    });
    Ok(result)
}

fn scan_directory(
    root: &Path,
    archived: bool,
    db_metadata: &HashMap<String, DbMetadata>,
    output: &mut Vec<ScannedThread>,
) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
            continue;
        }
        output.push(scan_rollout(path, root, archived, db_metadata)?);
    }
    Ok(())
}

fn scan_rollout(
    path: &Path,
    root: &Path,
    archived: bool,
    db_metadata: &HashMap<String, DbMetadata>,
) -> Result<ScannedThread> {
    let content = fs::read(path).with_context(|| format!("read rollout {}", path.display()))?;
    if content.is_empty() {
        return Err(anyhow!("empty rollout: {}", path.display()));
    }
    let reader = BufReader::new(content.as_slice());
    let mut id = None;
    let mut cwd = None;
    let mut timestamp = None;
    let mut source = None;
    let mut thread_source = None;
    let mut provider = None;
    let mut cli_version = None;
    let mut title = None;
    let mut first_user_message = None;

    for (index, line) in reader.lines().enumerate() {
        let line =
            line.with_context(|| format!("read line {} in {}", index + 1, path.display()))?;
        let value: Value = serde_json::from_str(&line)
            .with_context(|| format!("invalid JSON line {} in {}", index + 1, path.display()))?;
        match value.get("type").and_then(Value::as_str) {
            Some("session_meta") => {
                let payload = &value["payload"];
                id = payload.get("id").and_then(Value::as_str).map(str::to_owned);
                cwd = payload
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                timestamp = payload
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .and_then(parse_timestamp);
                if source.is_none() {
                    source = payload.get("source").and_then(source_value_to_string);
                }
                if thread_source.is_none() {
                    thread_source = payload
                        .get("thread_source")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                }
                provider = payload
                    .get("model_provider")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                cli_version = payload
                    .get("cli_version")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            Some("event_msg") => {
                let payload = &value["payload"];
                if payload.get("type").and_then(Value::as_str) == Some("thread_name_updated") {
                    title = payload
                        .get("thread_name")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                }
                if first_user_message.is_none()
                    && payload.get("type").and_then(Value::as_str) == Some("user_message")
                {
                    first_user_message = payload
                        .get("message")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                }
            }
            Some("response_item") if first_user_message.is_none() => {
                let payload = &value["payload"];
                if payload.get("type").and_then(Value::as_str) == Some("message")
                    && payload.get("role").and_then(Value::as_str) == Some("user")
                {
                    first_user_message = extract_message_text(payload);
                }
            }
            _ => {}
        }
    }

    let id = id
        .or_else(|| id_from_filename(path))
        .ok_or_else(|| anyhow!("cannot determine thread id for {}", path.display()))?;
    let metadata = db_metadata.get(&id);
    let sha256 = hex::encode(Sha256::digest(&content));
    let relative = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    let archive_path = if archived {
        format!("archived_sessions/{relative}")
    } else {
        format!("sessions/{relative}")
    };

    let record = ThreadRecord {
        id,
        title: metadata
            .map(|value| value.title.clone())
            .filter(|value| !value.is_empty())
            .or_else(|| title.filter(|value| !value.is_empty()))
            .or_else(|| {
                first_user_message
                    .as_deref()
                    .and_then(title_candidate_from_user_message)
                    .map(|value| summarize_title(&value))
            })
            .unwrap_or_default(),
        created_at: metadata
            .map(|value| value.created_at)
            .or(timestamp)
            .unwrap_or_default(),
        updated_at: metadata
            .map(|value| value.updated_at)
            .or(timestamp)
            .unwrap_or_default(),
        cwd: cwd
            .or_else(|| metadata.map(|value| value.cwd.clone()))
            .unwrap_or_default(),
        source: source
            .or_else(|| metadata.map(|value| value.source.clone()))
            .unwrap_or_else(|| "unknown".to_owned()),
        thread_source: thread_source
            .or_else(|| metadata.and_then(|value| value.thread_source.clone())),
        model_provider: provider
            .or_else(|| metadata.map(|value| value.model_provider.clone()))
            .unwrap_or_else(|| "openai".to_owned()),
        cli_version: cli_version
            .or_else(|| metadata.map(|value| value.cli_version.clone()))
            .unwrap_or_default(),
        archived,
        archive_path,
        sha256,
        byte_len: content.len() as u64,
        first_user_message: metadata
            .map(|value| value.first_user_message.clone())
            .filter(|value| !value.is_empty())
            .or(first_user_message)
            .unwrap_or_default(),
        sandbox_policy: metadata.and_then(|value| value.sandbox_policy.clone()),
        approval_mode: metadata.and_then(|value| value.approval_mode.clone()),
        model: metadata.and_then(|value| value.model.clone()),
        reasoning_effort: metadata.and_then(|value| value.reasoning_effort.clone()),
    };
    Ok(ScannedThread {
        record,
        source_path: path.to_path_buf(),
        content,
    })
}

fn load_db_metadata(path: &Path) -> Result<HashMap<String, DbMetadata>> {
    let connection = sqlite_adapter::open_readable(path)
        .with_context(|| format!("open state DB {}", path.display()))?;
    let has_threads: Option<i64> = connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='threads'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if has_threads.is_none() {
        return Ok(HashMap::new());
    }
    let columns = table_columns(&connection, "threads")?;
    let optional = |name: &str, fallback: &str| {
        if columns.iter().any(|column| column == name) {
            name.to_owned()
        } else {
            fallback.to_owned()
        }
    };
    let sql = format!(
        "SELECT id, title, created_at, updated_at, cwd, source, model_provider, \
         {}, {}, {}, {}, {}, {}, {} FROM threads",
        optional("cli_version", "''"),
        optional("first_user_message", "''"),
        optional("sandbox_policy", "NULL"),
        optional("approval_mode", "NULL"),
        optional("model", "NULL"),
        optional("reasoning_effort", "NULL"),
        optional("thread_source", "NULL"),
    );
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            DbMetadata {
                title: row.get(1)?,
                created_at: row.get(2)?,
                updated_at: row.get(3)?,
                cwd: row.get(4)?,
                source: row.get(5)?,
                model_provider: row.get(6)?,
                cli_version: row.get(7)?,
                first_user_message: row.get(8)?,
                sandbox_policy: row.get(9)?,
                approval_mode: row.get(10)?,
                model: row.get(11)?,
                reasoning_effort: row.get(12)?,
                thread_source: row.get(13)?,
            },
        ))
    })?;
    let mut result = HashMap::new();
    for row in rows {
        let (id, metadata) = row?;
        result.insert(id, metadata);
    }
    Ok(result)
}

fn table_columns(connection: &Connection, table: &str) -> Result<Vec<String>> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

fn parse_timestamp(value: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|value| value.timestamp())
}

fn id_from_filename(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let suffix = stem.rsplit('-').take(5).collect::<Vec<_>>();
    if suffix.len() != 5 {
        return None;
    }
    Some(suffix.into_iter().rev().collect::<Vec<_>>().join("-"))
}

const VSCODE_CONTEXT_PREFIX: &str = "# Context from my IDE setup:";
const CODEX_REQUEST_MARKER: &str = "my request for codex";

pub fn load_session_messages(path: &Path) -> Result<Vec<SessionMessage>> {
    let file =
        fs::File::open(path).with_context(|| format!("open session preview {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut messages = Vec::new();

    for line in reader.lines() {
        let Ok(line) = line else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("response_item") {
            continue;
        }
        let Some(payload) = value.get("payload") else {
            continue;
        };

        let payload_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
        let (role, content) = match payload_type {
            "message" => {
                let role = payload
                    .get("role")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_owned();
                let content = payload
                    .get("content")
                    .map(extract_content_text)
                    .unwrap_or_default();
                (role, content)
            }
            "function_call" => {
                let name = payload
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let arguments = payload
                    .get("arguments")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty());
                let content = match arguments {
                    Some(arguments) => format!("[Tool: {name}]\n{arguments}"),
                    None => format!("[Tool: {name}]"),
                };
                ("assistant".to_owned(), content)
            }
            "function_call_output" => {
                let content = payload
                    .get("output")
                    .map(extract_content_text)
                    .unwrap_or_default();
                ("tool".to_owned(), content)
            }
            _ => continue,
        };

        if content.trim().is_empty() {
            continue;
        }

        let timestamp = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_timestamp);
        messages.push(SessionMessage {
            role,
            content,
            timestamp,
        });
    }

    Ok(messages)
}

fn source_value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Object(_) => serde_json::to_string(value).ok(),
        _ => None,
    }
}

fn extract_message_text(payload: &Value) -> Option<String> {
    let text = payload
        .get("content")
        .map(extract_content_text)
        .unwrap_or_default();
    (!text.trim().is_empty()).then_some(text)
}

fn extract_content_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(extract_content_item)
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(map) => {
            for key in ["text", "input_text", "output_text"] {
                if let Some(text) = map.get(key).and_then(Value::as_str) {
                    return text.to_owned();
                }
            }
            map.get("content")
                .map(extract_content_text)
                .unwrap_or_default()
        }
        _ => String::new(),
    }
}

fn extract_content_item(item: &Value) -> Option<String> {
    let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
    if matches!(item_type, "tool_use" | "toolCall") {
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        return Some(format!("[Tool: {name}]"));
    }
    if item_type == "tool_result" {
        let content = item
            .get("content")
            .map(extract_content_text)
            .unwrap_or_default();
        return (!content.trim().is_empty()).then_some(content);
    }
    for key in ["text", "input_text", "output_text"] {
        if let Some(text) = item.get(key).and_then(Value::as_str) {
            return Some(text.to_owned());
        }
    }
    let nested = item
        .get("content")
        .map(extract_content_text)
        .unwrap_or_default();
    (!nested.trim().is_empty()).then_some(nested)
}

fn title_candidate_from_user_message(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty()
        || trimmed.starts_with("# AGENTS.md")
        || trimmed.starts_with("<environment_context>")
    {
        return None;
    }

    if trimmed.starts_with(VSCODE_CONTEXT_PREFIX) {
        return extract_codex_prompt_from_ide_context(trimmed);
    }

    Some(trimmed.to_owned())
}

fn extract_codex_prompt_from_ide_context(text: &str) -> Option<String> {
    let normalized = text.replace("\r\n", "\n");
    let lines = normalized.lines().collect::<Vec<_>>();
    let mut prompt = None;

    for (index, line) in lines.iter().enumerate() {
        let Some(inline_prompt) = codex_request_heading_payload(line) else {
            continue;
        };
        if !inline_prompt.is_empty() {
            prompt = Some(inline_prompt.to_owned());
            continue;
        }

        let following = lines[index + 1..].join("\n").trim().to_owned();
        prompt = (!following.is_empty()).then_some(following);
    }

    prompt
}

fn codex_request_heading_payload(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    if !trimmed.starts_with('#') {
        return None;
    }

    let heading = trimmed.trim_start_matches('#').trim_start();
    let lowered = heading.to_ascii_lowercase();
    if !lowered.starts_with(CODEX_REQUEST_MARKER) {
        return None;
    }

    let suffix = heading[CODEX_REQUEST_MARKER.len()..].trim_start();
    if suffix.is_empty() {
        return Some("");
    }

    let separator = suffix.chars().next()?;
    if !matches!(separator, ':' | '：' | '-' | '—') {
        return None;
    }

    Some(
        suffix
            .trim_start_matches(|c: char| c.is_whitespace() || matches!(c, ':' | '：' | '-' | '—'))
            .trim(),
    )
}

fn summarize_title(value: &str) -> String {
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    compact.chars().take(80).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn rejects_malformed_jsonl() {
        let home = TempDir::new().unwrap();
        let sessions = home.path().join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(
            sessions.join("rollout-2026-06-19T12-00-00-11111111-2222-3333-4444-555555555555.jsonl"),
            b"{not-json}\n",
        )
        .unwrap();
        assert!(scan_codex_home(home.path(), None).is_err());
    }

    #[test]
    fn scans_archived_unicode_session() {
        let home = TempDir::new().unwrap();
        let archived = home.path().join("archived_sessions");
        fs::create_dir_all(&archived).unwrap();
        let id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let line = serde_json::json!({
            "timestamp": "2026-06-19T04:00:00Z",
            "type": "session_meta",
            "payload": {
                "id": id,
                "timestamp": "2026-06-19T04:00:00Z",
                "cwd": "/项目/论文",
                "source": "vscode",
                "model_provider": "openai",
                "cli_version": "0.142.0"
            }
        });
        fs::write(
            archived.join(format!("rollout-2026-06-19T12-00-00-{id}.jsonl")),
            format!("{line}\n"),
        )
        .unwrap();
        let threads = scan_codex_home(home.path(), None).unwrap();
        assert_eq!(threads.len(), 1);
        assert!(threads[0].record.archived);
        assert_eq!(threads[0].record.cwd, "/项目/论文");
    }
    #[test]
    fn preserves_subagent_source_and_classifies_guardian() {
        let home = TempDir::new().unwrap();
        let sessions = home.path().join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        let id = "11111111-1111-4111-8111-111111111111";
        let line = serde_json::json!({
            "timestamp": "2026-07-01T00:00:00Z",
            "type": "session_meta",
            "payload": {
                "id": id,
                "cwd": "/tmp/project",
                "source": {"subagent": {"other": "guardian"}},
                "thread_source": "subagent",
                "model_provider": "openai"
            }
        });
        fs::write(
            sessions.join(format!("rollout-2026-07-01T00-00-00-{id}.jsonl")),
            format!("{line}\n"),
        )
        .unwrap();

        let threads = scan_codex_home(home.path(), None).unwrap();
        assert_eq!(threads.len(), 1);
        assert!(threads[0].record.is_internal_or_subagent());
        assert!(threads[0].record.source.contains("guardian"));
    }

    #[test]
    fn title_candidate_skips_injected_context_and_extracts_vscode_request() {
        assert!(title_candidate_from_user_message("<environment_context>\nfoo").is_none());
        assert!(title_candidate_from_user_message("# AGENTS.md instructions for /tmp").is_none());

        let text = "# Context from my IDE setup:\n## Open files:\n- main.rs\n\n## My request for Codex:\nFix the parser bug";
        assert_eq!(
            title_candidate_from_user_message(text).as_deref(),
            Some("Fix the parser bug")
        );
    }

    #[test]
    fn loads_full_preview_messages_and_tool_output() {
        let home = TempDir::new().unwrap();
        let path = home.path().join("preview.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"timestamp\":\"2026-07-01T00:00:00Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"hello\"}]}}\n",
                "{\"timestamp\":\"2026-07-01T00:00:01Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"function_call\",\"name\":\"shell\",\"arguments\":\"{\\\"cmd\\\":\\\"pwd\\\"}\"}}\n",
                "{\"timestamp\":\"2026-07-01T00:00:02Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"function_call_output\",\"output\":\"/tmp/project\"}}\n"
            ),
        )
        .unwrap();

        let messages = load_session_messages(&path).unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].content, "hello");
        assert!(messages[1].content.contains("[Tool: shell]"));
        assert_eq!(messages[2].role, "tool");
        assert_eq!(messages[2].content, "/tmp/project");
    }
}
