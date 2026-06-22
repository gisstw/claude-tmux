//! Map a tmux pane to its Claude Code session and report when that session was
//! last active (proxy for prompt-cache freshness / idle time).
//!
//! Chain: pane tty -> claude pid (the `~/.claude/sessions/<pid>.json` whose
//! process owns that tty) -> sessionId + cwd -> transcript `.jsonl`.
//!
//! We scan the tail of the transcript for the most recent *real* API turn
//! (type "user" or "assistant") rather than using the file mtime, because
//! Claude Code appends background entries – notably `subtype:"away_summary"`
//! at ~1 h of idle time – that reset the mtime without representing an actual
//! Anthropic API call. Those entries would permanently prevent the session from
//! appearing stale to the prompt-cache recache heuristic.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::{Duration, SystemTime};

/// Last-activity time for the Claude session on `tty`, if any.
///
/// Returns the timestamp of the most recent user/assistant turn in the
/// transcript, ignoring system-generated background entries.
pub fn last_activity_for_tty(tty: &str) -> Option<SystemTime> {
    let home = dirs::home_dir()?;
    let (session_id, cwd) = session_for_tty(&home, tty)?;
    let transcript = home
        .join(".claude/projects")
        .join(munge_path(&cwd))
        .join(format!("{session_id}.jsonl"));
    last_api_call_time(&transcript)
}

/// Scan the last 16 KB of the transcript for the most recent real API turn.
///
/// Skips entries whose `"type"` is `"system"`, `"mode"`, `"last-prompt"`,
/// `"custom-title"`, or `"agent-name"` – all of which are written by the
/// Claude Code harness in the background and do not represent Anthropic API
/// calls.  Falls back to the file mtime when no qualifying entry is found in
/// the tail window.
fn last_api_call_time(path: &Path) -> Option<SystemTime> {
    let mut file = fs::File::open(path).ok()?;
    let file_size = file.metadata().ok()?.len();

    const TAIL: u64 = 16384;
    let offset = file_size.saturating_sub(TAIL);
    file.seek(SeekFrom::Start(offset)).ok()?;

    let mut buf = Vec::with_capacity(TAIL as usize);
    file.read_to_end(&mut buf).ok()?;

    let text = String::from_utf8_lossy(&buf);

    // Walk lines in reverse so we stop at the first (= most recent) hit.
    for line in text.lines().rev() {
        if !line.contains("\"timestamp\"") {
            continue;
        }
        // Skip background / metadata entries written without an API call.
        if line.contains("\"type\":\"system\"")
            || line.contains("\"type\":\"mode\"")
            || line.contains("\"type\":\"last-prompt\"")
            || line.contains("\"type\":\"custom-title\"")
            || line.contains("\"type\":\"agent-name\"")
        {
            continue;
        }
        if let Some(t) = parse_timestamp(line) {
            return Some(t);
        }
    }

    // Fall back to mtime when the tail contains no qualifying entries.
    fs::metadata(path).ok()?.modified().ok()
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

/// Parse the `"timestamp":"YYYY-MM-DDTHH:MM:SS[.mmm]Z"` field from a jsonl
/// line into a `SystemTime`.  Handles UTC ISO-8601 only.
fn parse_timestamp(line: &str) -> Option<SystemTime> {
    // Find the timestamp value (first occurrence on the line).
    let after = line.split("\"timestamp\":\"").nth(1)?;
    let ts = &after[..after.find('"')?];
    if ts.len() < 19 {
        return None;
    }
    let year: u64 = ts[0..4].parse().ok()?;
    let month: u64 = ts[5..7].parse().ok()?;
    let day: u64 = ts[8..10].parse().ok()?;
    let hour: u64 = ts[11..13].parse().ok()?;
    let min: u64 = ts[14..16].parse().ok()?;
    let sec: u64 = ts[17..19].parse().ok()?;

    let unix = civil_to_unix(year, month, day, hour, min, sec)?;
    SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(unix))
}

/// Convert a UTC civil datetime to a Unix timestamp (seconds since 1970-01-01).
/// Valid for years 1970-2099 (the range Claude Code transcripts will ever have).
fn civil_to_unix(year: u64, month: u64, day: u64, h: u64, m: u64, s: u64) -> Option<u64> {
    const DAYS_BEFORE_MONTH: [u64; 13] = [0, 0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let is_leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let leap_day: u64 = if is_leap && month > 2 { 1 } else { 0 };

    // Days from 1970-01-01 to the start of the given year.
    let y = year.checked_sub(1970)?;
    let days_in_years = y * 365 + (y + 3) / 4 - (y + 99) / 100 + (y + 399) / 400;

    let days = days_in_years
        + *DAYS_BEFORE_MONTH.get(month as usize)?
        + leap_day
        + day
        - 1;

    Some(days * 86400 + h * 3600 + m * 60 + s)
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

    #[test]
    fn timestamp_parse() {
        // 2026-06-21T03:27:38Z  → unix 1750473858 + 21*86400+3*3600+27*60+38
        // quick sanity: epoch + some large number is in the future relative to 2020
        let line = r#"{"type":"assistant","timestamp":"2026-06-21T03:27:38.361Z"}"#;
        let t = parse_timestamp(line).expect("should parse");
        // Should be roughly 2026 (seconds since epoch ≈ 1.75 billion)
        let unix = t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
        assert!(unix > 1_700_000_000, "parsed timestamp too old: {unix}");
        assert!(unix < 2_000_000_000, "parsed timestamp too far: {unix}");
    }

    #[test]
    fn civil_to_unix_epoch() {
        assert_eq!(civil_to_unix(1970, 1, 1, 0, 0, 0), Some(0));
        assert_eq!(civil_to_unix(1970, 1, 2, 0, 0, 0), Some(86400));
        // 2024 is a leap year; 2024-03-01 should include the leap day
        let leap = civil_to_unix(2024, 3, 1, 0, 0, 0).unwrap();
        let non_leap = civil_to_unix(2023, 3, 1, 0, 0, 0).unwrap();
        assert_eq!(leap - non_leap, 366 * 86400);
    }
}
