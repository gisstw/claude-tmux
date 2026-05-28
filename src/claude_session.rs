//! Map a tmux pane to its Claude Code session and report when that session was
//! last active (proxy for prompt-cache freshness / idle time).
//!
//! Chain: pane tty -> claude pid (the `~/.claude/sessions/<pid>.json` whose
//! process owns that tty) -> sessionId + cwd -> transcript `.jsonl` mtime.
//! Every API turn/tool event appends to the transcript, so its mtime freezes
//! exactly while the session is idle. Coupled to Claude Code's on-disk layout;
//! see PATCHES.md for what to check if a Claude Code upgrade breaks this.

use std::fs;
use std::path::Path;
use std::time::SystemTime;

/// Last-activity time (transcript mtime) for the Claude session on `tty`, if any.
pub fn last_activity_for_tty(tty: &str) -> Option<SystemTime> {
    let home = dirs::home_dir()?;
    let (session_id, cwd) = session_for_tty(&home, tty)?;
    let transcript = home
        .join(".claude/projects")
        .join(munge_path(&cwd))
        .join(format!("{session_id}.jsonl"));
    fs::metadata(transcript).ok()?.modified().ok()
}

/// Find the `sessionId` + `cwd` of the Claude process whose stdin is `tty`.
fn session_for_tty(home: &Path, tty: &str) -> Option<(String, String)> {
    for entry in fs::read_dir(home.join(".claude/sessions")).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(pid) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if !pid.bytes().all(|b| b.is_ascii_digit()) || pid_tty(pid).as_deref() != Some(tty) {
            continue;
        }
        let text = fs::read_to_string(&path).ok()?;
        return Some((json_str(&text, "sessionId")?, json_str(&text, "cwd")?));
    }
    None
}

/// Controlling terminal of `pid` via /proc/<pid>/fd/0 (e.g. "/dev/pts/3").
fn pid_tty(pid: &str) -> Option<String> {
    let link = fs::read_link(format!("/proc/{pid}/fd/0")).ok()?;
    Some(link.to_string_lossy().into_owned())
}

/// Claude Code names a project dir by replacing '/' and '.' in the cwd with '-'.
fn munge_path(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c == '/' || c == '.' { '-' } else { c })
        .collect()
}

/// Extract the string value of a top-level `"key": "value"` pair.
/// Assumes the simple, escape-free values Claude Code writes (UUIDs, paths).
fn json_str(text: &str, key: &str) -> Option<String> {
    let after_key = &text[text.find(&format!("\"{key}\""))? + key.len() + 2..];
    let after_colon = &after_key[after_key.find(':')? + 1..];
    let start = after_colon.find('"')? + 1;
    let len = after_colon[start..].find('"')?;
    Some(after_colon[start..start + len].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn munge() {
        assert_eq!(munge_path("/home/www"), "-home-www");
        assert_eq!(munge_path("/home/www/pms-dev"), "-home-www-pms-dev");
    }

    #[test]
    fn extract() {
        let t = r#"{"pid":123,"sessionId":"ab-cd","cwd":"/home/www","status":"busy"}"#;
        assert_eq!(json_str(t, "sessionId").as_deref(), Some("ab-cd"));
        assert_eq!(json_str(t, "cwd").as_deref(), Some("/home/www"));
        assert_eq!(json_str(t, "missing"), None);
    }
}
