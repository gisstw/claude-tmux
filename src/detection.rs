use crate::session::ClaudeCodeStatus;

/// Detect Claude Code status when content has NOT changed since the last check.
///
/// Identical to [`detect_status`]. Originally this skipped the Working check on
/// the assumption that Working is always caught by content-change detection in
/// `App::tick_status`. That assumption broke for sessions deep in extended
/// thinking: when two captures land on the same spinner frame, the content
/// compares equal, this path runs, and a working screen (which still shows the
/// `❯` input field) was misclassified as Idle — then displayed a huge idle time
/// because the transcript isn't written during thinking. Recognizing the
/// working markers here fixes that.
pub fn detect_static_status(content: &str) -> ClaudeCodeStatus {
    detect_status(content)
}

/// Detect Claude Code status from pane content.
pub fn detect_status(content: &str) -> ClaudeCodeStatus {
    if content.contains("[y/n]") || content.contains("[Y/n]") {
        return ClaudeCodeStatus::WaitingInput;
    }

    // A working/thinking screen still renders the `❯` input field, so check the
    // working markers BEFORE has_input_field — otherwise a busy session is
    // wrongly read as Idle.
    if is_working_screen(content) {
        return ClaudeCodeStatus::Working;
    }

    if has_input_field(content) {
        return ClaudeCodeStatus::Idle;
    }

    ClaudeCodeStatus::Unknown
}

/// Whether the pane shows Claude actively working or thinking.
///
/// Claude Code's status line during work looks like one of:
/// ```text
/// ✻ Cogitating… (12s · ↓ 1.2k tokens · esc to interrupt)
/// ✶ Computing… (24s · thinking more with xhigh effort)
/// ```
/// The reliable, version-stable markers are the interrupt hint (generation /
/// tool execution) and the thinking-effort suffix (extended thinking, which
/// shows no interrupt hint). Neither appears on an idle screen. We match
/// `"to interrupt"` rather than `"ctrl+c to interrupt"` so the modern
/// `"esc to interrupt"` hint is also covered.
fn is_working_screen(content: &str) -> bool {
    content.contains("to interrupt") || content.contains(" effort)")
}

/// Detect input field: prompt line (❯) with border directly above it.
fn has_input_field(content: &str) -> bool {
    let lines: Vec<&str> = content.lines().collect();

    for (i, line) in lines.iter().enumerate() {
        if line.contains('❯') {
            // Check if line above is a border
            if i > 0 && lines[i - 1].contains('─') {
                return true;
            }
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_working() {
        // Border directly above prompt
        let content = "* (ctrl+c to interrupt)\n─────\n❯ hello";
        assert_eq!(detect_status(content), ClaudeCodeStatus::Working);
    }

    #[test]
    fn test_idle() {
        // Border directly above prompt
        let content = "● Done\n─────\n❯ hello";
        assert_eq!(detect_status(content), ClaudeCodeStatus::Idle);
    }

    #[test]
    fn test_no_border_above_prompt() {
        // Border exists but not directly above prompt - should be unknown
        let content = "─────\nsome text\n❯ hello";
        assert_eq!(detect_status(content), ClaudeCodeStatus::Unknown);
    }

    #[test]
    fn test_waiting_input() {
        let content = "Delete files? [y/n]";
        assert_eq!(detect_status(content), ClaudeCodeStatus::WaitingInput);
    }

    #[test]
    fn test_unknown() {
        let content = "random stuff";
        assert_eq!(detect_status(content), ClaudeCodeStatus::Unknown);
    }

    // Real thinking screen captured from a live session. No "ctrl+c to
    // interrupt" hint, only the effort suffix — but still working.
    const THINKING_EFFORT: &str =
        "✶ Computing… (24s · thinking more with xhigh effort)\n─────\n❯ ";
    // Generation screen with the modern "esc to interrupt" hint (not ctrl+c).
    const WORKING_ESC: &str =
        "✻ Cogitating… (12s · ↓ 1.2k tokens · esc to interrupt)\n─────\n❯ ";

    #[test]
    fn test_thinking_effort_is_working() {
        // Was previously misclassified as Idle (has ❯ field, no ctrl+c).
        assert_eq!(detect_status(THINKING_EFFORT), ClaudeCodeStatus::Working);
    }

    #[test]
    fn test_esc_interrupt_is_working() {
        // "esc to interrupt" must count as working, not just "ctrl+c".
        assert_eq!(detect_status(WORKING_ESC), ClaudeCodeStatus::Working);
    }

    #[test]
    fn test_static_status_recognizes_working() {
        // The key fix: even on an unchanged frame, a working screen is not Idle.
        assert_eq!(
            detect_static_status(THINKING_EFFORT),
            ClaudeCodeStatus::Working
        );
        assert_eq!(detect_static_status(WORKING_ESC), ClaudeCodeStatus::Working);
    }

    #[test]
    fn test_static_status_still_idle_when_truly_idle() {
        // A genuinely idle screen (prompt + border, no working markers).
        let idle = "● Done\n─────\n❯ ";
        assert_eq!(detect_static_status(idle), ClaudeCodeStatus::Idle);
    }
}
