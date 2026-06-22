//! Map a tmux pane to its AI tool session and report when that session was
//! last active (proxy for prompt-cache freshness / idle time).
//!
//! ## Claude Code
//! Chain: pane tty -> claude pid (the `~/.claude/sessions/<pid>.json` whose
//! process owns that tty) -> sessionId + cwd -> transcript `.jsonl`.
//!
//! We scan the tail of the transcript for the most recent *real* API turn
//! (type "user" or "assistant") rather than using the file mtime, because
//! Claude Code appends background entries – notably `subtype:"away_summary"`
//! at ~1 h of idle time – that reset the mtime without representing an actual
//! Anthropic API call. Those entries would permanently prevent the session from
//! appearing stale to the prompt-cache recache heuristic.
//!
//! ## Codex
//! Chain: pane tty -> pid -> process starttime (boot-relative jiffies via
//! `/proc/<pid>/stat`) -> convert to wall-clock UTC -> scan
//! `~/.codex/sessions/YYYY/MM/DD/` for the `rollout-*.jsonl` whose filename
//! datetime is nearest-after the process start -> read last lines for the
//! most recent `"timestamp"` entry.
//!
//! ## OpenCode
//! Chain: pane tty -> pid -> `/proc/<pid>/cwd` symlink -> working directory ->
//! query `~/.local/share/opencode/opencode.db` for
//! `MAX(message.time_updated)` where `session.directory = cwd`.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::session::ToolType;

/// Last-activity time for the AI session on `tty`, dispatched by `tool_type`.
///
/// `cwd` is used by the OpenCode path (working directory of the process).
/// For Claude and Codex it is derived internally from the tty/pid chain.
pub fn last_activity_for_tty(tty: &str, tool_type: ToolType, cwd: &str) -> Option<SystemTime> {
    match tool_type {
        ToolType::Claude => last_activity_claude(tty),
        ToolType::Codex => last_activity_codex(tty),
        ToolType::OpenCode => last_activity_opencode(cwd),
    }
}

// ---------------------------------------------------------------------------
// Claude Code
// ---------------------------------------------------------------------------

fn last_activity_claude(tty: &str) -> Option<SystemTime> {
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

// ---------------------------------------------------------------------------
// Codex
// ---------------------------------------------------------------------------

/// Last-activity time for a Codex session running on `tty`.
///
/// Steps:
/// 1. Resolve tty → pid
/// 2. Read `/proc/<pid>/stat` to get the process start time (jiffies since boot)
/// 3. Convert to a wall-clock UTC instant
/// 4. Scan `~/.codex/sessions/YYYY/MM/DD/` (in LOCAL time, since codex names
///    directories and files using local time) for the `rollout-*.jsonl` file
///    whose embedded datetime is earliest-on-or-after the process start
/// 5. Read the tail of that file for the most recent `"timestamp"` entry
fn last_activity_codex(tty: &str) -> Option<SystemTime> {
    let home = dirs::home_dir()?;

    // Find the PID that owns this TTY.
    let pid = tty_to_pid(tty)?;

    // Get the process start time as a SystemTime.
    let proc_start = pid_start_time(&pid)?;

    // Find the matching codex session file.
    let session_file = find_codex_session(&home, proc_start)?;

    // Return the most recent timestamp from the tail of that file.
    last_timestamp_in_file(&session_file)
        .or_else(|| fs::metadata(&session_file).ok()?.modified().ok())
}

/// Scan /proc to find the PID whose controlling terminal (stdin fd) is `tty`.
///
/// Returns the **highest** matching PID (most recently started process), which
/// is the foreground process (codex/opencode) rather than the parent shell that
/// also holds the same TTY.
fn tty_to_pid(tty: &str) -> Option<String> {
    let mut best: Option<u64> = None;
    for entry in fs::read_dir("/proc").ok()?.flatten() {
        let name = entry.file_name();
        let pid_str = name.to_string_lossy();
        if !pid_str.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if pid_tty(&pid_str).as_deref() == Some(tty) {
            if let Ok(pid_num) = pid_str.parse::<u64>() {
                if best.map_or(true, |b| pid_num > b) {
                    best = Some(pid_num);
                }
            }
        }
    }
    best.map(|p| p.to_string())
}

/// Return the wall-clock `SystemTime` when `pid` was started.
///
/// Reads field 22 (1-based, 0-indexed = 21) from `/proc/<pid>/stat` —
/// the process start time in clock ticks since system boot — then adds that
/// to the system boot time (from `/proc/stat btime`).
fn pid_start_time(pid: &str) -> Option<SystemTime> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;

    // Field 22 is starttime (ticks since boot). Fields are space-separated,
    // but field 2 (comm) may contain spaces inside parentheses. Skip past the
    // closing ')' to avoid being tricked by process names with spaces.
    let after_comm = stat.rsplit(')').next()?;
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    // After comm, the fields start at position 0 = field 3 (state).
    // Field 22 is at index 22 - 3 = 19 in this sub-slice.
    let starttime_ticks: u64 = fields.get(19)?.parse().ok()?;

    let hz = ticks_per_second();
    let boot_time = system_boot_time()?;

    let start_secs = starttime_ticks / hz;
    let start_subsec = Duration::from_millis((starttime_ticks % hz) * 1000 / hz);

    boot_time
        .checked_add(Duration::from_secs(start_secs))?
        .checked_add(start_subsec)
}

/// Read `CONFIG_HZ` equivalent: number of clock ticks per second.
/// Tries `sysconf(_SC_CLK_TCK)`; falls back to 100.
fn ticks_per_second() -> u64 {
    // SAFETY: sysconf is always safe to call with a valid name constant.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if hz > 0 { hz as u64 } else { 100 }
}

/// Read the system boot time from `/proc/stat` (`btime` line).
fn system_boot_time() -> Option<SystemTime> {
    let stat = fs::read_to_string("/proc/stat").ok()?;
    for line in stat.lines() {
        if let Some(rest) = line.strip_prefix("btime ") {
            let secs: u64 = rest.trim().parse().ok()?;
            return SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(secs));
        }
    }
    None
}

/// Find the codex session `.jsonl` file that corresponds to a process that
/// started at `proc_start`.
///
/// Codex places files under `~/.codex/sessions/YYYY/MM/DD/` using LOCAL time
/// and names them `rollout-YYYY-MM-DDTHH-MM-SS-<uuid>.jsonl`.  We convert
/// `proc_start` to a local-time civil date and scan the candidates from that
/// day (and the day before as a safety margin) picking the file whose name
/// datetime is the earliest one that is >= `proc_start`.
fn find_codex_session(home: &Path, proc_start: SystemTime) -> Option<std::path::PathBuf> {
    let base = home.join(".codex/sessions");

    // Convert proc_start to seconds since epoch, then to local civil date.
    // We use the UTC offset via libc localtime_r to avoid a chrono dependency.
    let start_unix = proc_start
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_secs() as libc::time_t;

    // Get local year/month/day for proc_start and one day earlier.
    let (y, mo, d) = unix_to_local_date(start_unix)?;

    let mut candidates: Vec<(SystemTime, std::path::PathBuf)> = Vec::new();

    // Scan today and yesterday (in local time) to catch sessions that started
    // just before midnight.
    for day_offset in 0u32..=1 {
        let probe_unix = start_unix.saturating_sub((day_offset as libc::time_t) * 86400);
        let (py, pm, pd) = match unix_to_local_date(probe_unix) {
            Some(v) => v,
            None => continue,
        };
        let dir = base.join(format!("{py:04}/{pm:02}/{pd:02}"));
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            if let Some(file_time) = codex_filename_to_time(&path) {
                candidates.push((file_time, path));
            }
        }
        let _ = (y, mo, d); // suppress unused warnings
    }

    // Also check tomorrow (proc_start close to midnight edge case).
    {
        let probe_unix = start_unix.saturating_add(86400);
        if let Some((ny, nm, nd)) = unix_to_local_date(probe_unix) {
            let dir = base.join(format!("{ny:04}/{nm:02}/{nd:02}"));
            if let Ok(entries) = fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                        continue;
                    }
                    if let Some(file_time) = codex_filename_to_time(&path) {
                        candidates.push((file_time, path));
                    }
                }
            }
        }
    }

    if candidates.is_empty() {
        return None;
    }

    // Among files that started at or after proc_start, pick the earliest one
    // (= the session that was launched right after the process started).
    // If none is >= proc_start, pick the most recent one overall as fallback.
    candidates.sort_by_key(|(t, _)| *t);

    if let Some((_, path)) = candidates.iter().find(|(t, _)| *t >= proc_start) {
        return Some(path.clone());
    }

    // Fallback: most recent file (last element after sort)
    candidates.into_iter().last().map(|(_, p)| p)
}

/// Parse `YYYY-MM-DDTHH-MM-SS` out of a codex session filename and return
/// the corresponding UTC `SystemTime`.
///
/// Codex filenames use LOCAL time, so we interpret the parsed fields as local
/// time and convert to UTC via `libc::mktime`.
fn codex_filename_to_time(path: &Path) -> Option<SystemTime> {
    let name = path.file_stem()?.to_string_lossy();
    // "rollout-2026-06-13T10-30-44-<uuid>"
    let rest = name.strip_prefix("rollout-")?;
    // rest: "2026-06-13T10-30-44-..."
    if rest.len() < 19 {
        return None;
    }
    let year: i32 = rest[0..4].parse().ok()?;
    let month: i32 = rest[5..7].parse().ok()?;
    let day: i32 = rest[8..10].parse().ok()?;
    let hour: i32 = rest[11..13].parse().ok()?;
    let min: i32 = rest[14..16].parse().ok()?;
    let sec: i32 = rest[17..19].parse().ok()?;

    // Use libc::mktime to convert local time to UTC epoch.
    let mut tm = unsafe { std::mem::zeroed::<libc::tm>() };
    tm.tm_year = year - 1900;
    tm.tm_mon = month - 1;
    tm.tm_mday = day;
    tm.tm_hour = hour;
    tm.tm_min = min;
    tm.tm_sec = sec;
    tm.tm_isdst = -1; // let libc figure out DST

    let unix = unsafe { libc::mktime(&mut tm) };
    if unix < 0 {
        return None;
    }

    SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(unix as u64))
}

/// Convert a Unix timestamp to a local-time `(year, month, day)` tuple.
fn unix_to_local_date(unix: libc::time_t) -> Option<(i32, u32, u32)> {
    let mut tm = unsafe { std::mem::zeroed::<libc::tm>() };
    let result = unsafe { libc::localtime_r(&unix, &mut tm) };
    if result.is_null() {
        return None;
    }
    Some((tm.tm_year + 1900, (tm.tm_mon + 1) as u32, tm.tm_mday as u32))
}

/// Read the tail of a jsonl file and return the most recent `"timestamp"` value.
fn last_timestamp_in_file(path: &Path) -> Option<SystemTime> {
    let mut file = fs::File::open(path).ok()?;
    let file_size = file.metadata().ok()?.len();

    const TAIL: u64 = 8192;
    let offset = file_size.saturating_sub(TAIL);
    file.seek(SeekFrom::Start(offset)).ok()?;

    let mut buf = Vec::with_capacity(TAIL as usize);
    file.read_to_end(&mut buf).ok()?;

    let text = String::from_utf8_lossy(&buf);
    for line in text.lines().rev() {
        if line.contains("\"timestamp\"") {
            if let Some(t) = parse_timestamp(line) {
                return Some(t);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// OpenCode
// ---------------------------------------------------------------------------

/// Last-activity time for an OpenCode session with working directory `cwd`.
///
/// Queries `~/.local/share/opencode/opencode.db` for the most recent
/// `message.time_updated` (milliseconds since epoch) among sessions whose
/// `directory` matches `cwd`.
fn last_activity_opencode(cwd: &str) -> Option<SystemTime> {
    if cwd.is_empty() {
        return None;
    }

    let db_path = dirs::home_dir()?
        .join(".local/share/opencode/opencode.db");

    if !db_path.exists() {
        return None;
    }

    let conn = rusqlite::Connection::open(&db_path).ok()?;

    // Find the most recent message timestamp for any session in this directory.
    let ms: Option<i64> = conn
        .query_row(
            "SELECT MAX(m.time_updated) \
             FROM message m \
             JOIN session s ON m.session_id = s.id \
             WHERE s.directory = ?1",
            rusqlite::params![cwd],
            |row| row.get(0),
        )
        .ok()
        .flatten();

    let ms = ms?;
    if ms <= 0 {
        return None;
    }

    SystemTime::UNIX_EPOCH.checked_add(Duration::from_millis(ms as u64))
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Controlling terminal of `pid` via /proc/<pid>/fd/0 (e.g. "/dev/pts/3").
fn pid_tty(pid: &str) -> Option<String> {
    let link = fs::read_link(format!("/proc/{pid}/fd/0")).ok()?;
    Some(link.to_string_lossy().into_owned())
}

/// Working directory of `pid` via /proc/<pid>/cwd symlink.
pub fn pid_cwd(pid: &str) -> Option<String> {
    let link = fs::read_link(format!("/proc/{pid}/cwd")).ok()?;
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

    #[test]
    fn codex_filename_parse() {
        use std::path::PathBuf;
        // Verify that a known codex filename parses to a reasonable timestamp
        let path = PathBuf::from(
            "/home/www/.codex/sessions/2026/06/13/rollout-2026-06-13T10-30-44-019ebed1-3ca3-7473-a240-c5d4b167d342.jsonl"
        );
        let t = codex_filename_to_time(&path);
        assert!(t.is_some(), "should parse codex filename datetime");
        let unix = t.unwrap()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // 2026-06-13 local = somewhere around 1749739844 UTC (±timezone)
        assert!(unix > 1_700_000_000, "parsed time too old: {unix}");
        assert!(unix < 2_000_000_000, "parsed time too far: {unix}");
    }

    #[test]
    fn opencode_full_path() {
        // Direct call simulating what tick_status does for the 'open' session
        let result = last_activity_for_tty("/dev/pts/10", crate::session::ToolType::OpenCode, "/home/www");
        eprintln!("last_activity_for_tty(opencode, /home/www): {:?}", result);
        // If this is None, last_activity_opencode failed — check DB open/query
        let direct = last_activity_opencode("/home/www");
        eprintln!("last_activity_opencode directly: {:?}", direct);
    }

    #[test]
    fn opencode_db_query() {
        let db_path = dirs::home_dir().unwrap().join(".local/share/opencode/opencode.db");
        if !db_path.exists() {
            eprintln!("SKIP: DB not found at {:?}", db_path);
            return;
        }
        let conn = rusqlite::Connection::open(&db_path);
        eprintln!("open result: {:?}", conn.as_ref().err());
        let conn = conn.expect("should open DB");

        let result: rusqlite::Result<Option<i64>> = conn.query_row(
            "SELECT MAX(m.time_updated) FROM message m JOIN session s ON m.session_id=s.id WHERE s.directory=?1",
            rusqlite::params!["/home/www"],
            |row| row.get(0),
        );
        eprintln!("query result: {:?}", result);
        let ms = result.expect("query ok").expect("should have row");
        assert!(ms > 1_000_000_000_000, "timestamp looks wrong: {ms}");
    }
}
