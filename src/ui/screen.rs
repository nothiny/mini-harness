//! Full-screen terminal UI (design §15, plan §13.3).
//!
//! Layout:
//!
//! ```text
//! ┌────────────────────────────────────────────────┐
//! │ session sess_ab12…  state RunningTurn  q1      │  header
//! │ event seq=3 turn.started                       │  scrollable,
//! │ …folded tool output…                           │  wrapped body
//! ├────────────────────────────────────────────────┤
//! │ > type here_                                   │  input line
//! └────────────────────────────────────────────────┘
//! ```
//!
//! Keys: Enter submits (queueing behind a running turn), Ctrl-C cancels the
//! active turn or exits when idle, Ctrl-D exits, y/n answer approvals,
//! f toggles output folding, r redraws, j/k and PgUp/PgDn scroll, g/G jump.
//! The UI is a pure event projection: it never owns runtime state (§13.3).

use super::jsonl_client::JsonlClient;
use crate::runtime::SessionId;
use serde_json::{Value, json};
use std::io::Write as _;
use tokio::io::AsyncReadExt as _;
use unicode_width::{UnicodeWidthChar as _, UnicodeWidthStr as _};

const FOLD_LIMIT: usize = 160;
const MAX_LINES: usize = 5000;

/// One rendered body line, possibly folded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BodyLine {
    pub(crate) text: String,
    /// Present when folding applies: the unfolded text.
    pub(crate) full: Option<String>,
}

/// Builds a body line from a protocol event projection.
pub(crate) fn event_line(event: &Value) -> BodyLine {
    let kind = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let mut summary = String::new();
    for key in [
        "tool",
        "tool_call_id",
        "approved",
        "reason",
        "outcome",
        "result",
    ] {
        if let Some(value) = event.get(key) {
            if !summary.is_empty() {
                summary.push(' ');
            }
            summary.push_str(&format!("{key}={}", compact(value)));
        }
    }
    let text = if summary.is_empty() {
        kind.to_string()
    } else {
        format!("{kind}  {summary}")
    };
    // Fold any over-long projection; the fold is visual only and `f` unfolds.
    if text.chars().count() > FOLD_LIMIT {
        let folded: String = text.chars().take(FOLD_LIMIT).collect();
        BodyLine {
            text: format!("{folded} … [+f]"),
            full: Some(text),
        }
    } else {
        BodyLine { text, full: None }
    }
}

fn compact(value: &Value) -> String {
    let raw = match value {
        Value::String(value) => value.clone(),
        _ => value.to_string(),
    };
    raw.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(240)
        .collect()
}

/// Wraps one logical line to visual rows of `width` columns, honouring
/// double-width (CJK) characters.
pub(crate) fn wrap_line(line: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut current = String::new();
    let mut used = 0usize;
    for character in line.chars() {
        let cell = character.width().unwrap_or(0).max(1);
        if used + cell > width {
            rows.push(std::mem::take(&mut current));
            used = 0;
        }
        current.push(character);
        used += cell;
    }
    rows.push(current);
    rows
}

/// Pure scroll+wrap model for the body pane.
#[derive(Default)]
pub(crate) struct Screen {
    pub(crate) lines: Vec<BodyLine>,
    pub(crate) scroll: usize,
    pub(crate) follow: bool,
    pub(crate) folded: bool,
}

impl Screen {
    pub(crate) fn push(&mut self, line: BodyLine) {
        self.lines.push(line);
        if self.lines.len() > MAX_LINES {
            let drop = self.lines.len() - MAX_LINES;
            self.lines.drain(..drop);
            self.scroll = self.scroll.saturating_sub(drop);
        }
    }

    pub(crate) fn visual_rows(&self, width: usize) -> Vec<(usize, String)> {
        // (original line index, visual row)
        let mut rows = Vec::new();
        for (index, line) in self.lines.iter().enumerate() {
            let text = if self.folded {
                line.text.as_str()
            } else {
                line.full.as_deref().unwrap_or(line.text.as_str())
            };
            for row in wrap_line(text, width) {
                rows.push((index, row));
            }
        }
        rows
    }

    /// Renders the body into `height` rows. Returns (rows, follow-pinned).
    pub(crate) fn render(&mut self, width: usize, height: usize) -> Vec<String> {
        let rows = self.visual_rows(width);
        let total = rows.len();
        if self.follow || self.scroll + height >= total {
            self.scroll = total.saturating_sub(height);
        }
        let start = self.scroll.min(total.saturating_sub(height));
        rows[start..start + height.min(total.saturating_sub(start))]
            .iter()
            .map(|(_, row)| row.clone())
            .collect()
    }

    pub(crate) fn scroll_by(&mut self, delta: i64, height: usize, width: usize) {
        self.follow = false;
        let total = self.visual_rows(width).len() as i64;
        let max = (total - height as i64).max(0);
        self.scroll = (self.scroll as i64 + delta).clamp(0, max) as usize;
    }

    pub(crate) fn jump_to_end(&mut self, height: usize, width: usize) {
        let total = self.visual_rows(width).len();
        self.scroll = total.saturating_sub(height);
        self.follow = true;
    }
}

pub(crate) fn header(
    session: &SessionId,
    state: &str,
    queued: usize,
    approval: Option<&str>,
    cols: usize,
) -> String {
    let mut text = format!("session {session}  {state}");
    if queued > 0 {
        text.push_str(&format!("  queued:{queued}"));
    }
    if let Some(call) = approval {
        text.push_str(&format!("  approve {call}? [y/n]"));
    }
    truncate_width(&text, cols)
}

fn truncate_width(text: &str, cols: usize) -> String {
    if text.width() <= cols {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0).max(1);
        if used + w > cols.saturating_sub(1) {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

/// Drives the full-screen UI. Only called when stdin is a terminal.
pub(crate) async fn run(options: super::tui::TuiOptions) -> Result<(), String> {
    let client = JsonlClient::spawn().await?;
    let split = client.into_split().await?;
    let mut requester = split.requester;
    let mut messages = split.messages;
    let result = run_screen(&mut requester, &mut messages, options).await;
    match result {
        Ok(()) => {
            let _ = requester.request("session.shutdown", json!({})).await;
            Ok(())
        }
        Err(error) => Err(error),
    }
}

async fn run_screen(
    requester: &mut super::jsonl_client::JsonlRequester,
    messages: &mut tokio::sync::mpsc::Receiver<Value>,
    options: super::tui::TuiOptions,
) -> Result<(), String> {
    let negotiated = requester
        .request("protocol.negotiate", json!({"version": 2}))
        .await?;
    if negotiated["result"]["capabilities"]["event_notifications"] != Value::Bool(true) {
        return Err("JSONL server does not advertise event notifications".into());
    }

    let raw = super::terminal::RawMode::enter()?;
    let mut resize = super::terminal::RawMode::resize_signals()?;
    let (session_id, mut history) = open_session(requester, messages, &options).await?;

    let mut screen = Screen {
        follow: true,
        folded: true,
        ..Screen::default()
    };
    for value in history.drain(..) {
        if let Some(line) = projected_event(&value) {
            screen.push(event_line(&line));
        }
    }

    let mut input = String::new();
    let mut pending_approval: Option<String> = None;
    let mut active_turn: Option<String> = None;
    let mut queued = 0usize;
    let mut status = "Idle".to_owned();
    let mut running = true;

    let mut stdin = tokio::io::stdin();

    let mut input_bytes = [0u8; 64];
    let mut escape = Vec::new();
    let mut utf8 = Vec::new();

    while running {
        let (rows, cols) = super::terminal::RawMode::size();
        draw(
            rows,
            cols,
            &screen,
            &session_id,
            &status,
            queued,
            pending_approval.as_deref(),
            &input,
        );
        tokio::select! {
            read = stdin.read(&mut input_bytes) => {
                let read = read.map_err(|error| format!("read UI input: {error}"))?;
                if read == 0 {
                    break;
                }
                for key in keys(&input_bytes[..read], &mut escape, &mut utf8) {
                    match key {
                        Key::Char('\r') | Key::Char('\n') => {
                            let line = std::mem::take(&mut input);
                            if let Err(error) = submit_line(
                                requester,
                                &session_id,
                                &line,
                                &mut active_turn,
                                &mut queued,
                                &mut status,
                            )
                            .await
                            {
                                status = format!("error: {error}");
                            }
                        }
                        Key::Char('\x03') => {
                            // Ctrl-C: cancel the active turn, exit when idle.
                            if let Some(turn) = active_turn.clone() {
                                let _ = requester
                                    .request(
                                        "turn.cancel",
                                        json!({"session_id": session_id, "turn_id": turn}),
                                    )
                                    .await;
                                status = "cancel requested".into();
                            } else {
                                running = false;
                            }
                        }
                        Key::Char('\x04') => running = false,
                        Key::Char('y') | Key::Char('n') if pending_approval.is_some() => {
                            let approval = pending_approval.take();
                            if let Some((turn, call)) = approval.and_then(|a| decode_approval(&a)) {
                                let approved = matches!(key, Key::Char('y'));
                                let result = requester
                                    .request(
                                        "approval.respond",
                                        json!({
                                            "session_id": session_id,
                                            "turn_id": turn,
                                            "call_id": call,
                                            "approved": approved,
                                        }),
                                    )
                                    .await;
                                if let Err(error) = result {
                                    status = format!("approval failed: {error}");
                                }
                            }
                        }
                        Key::Char('f') => screen.folded = !screen.folded,
                        Key::Char('r') => status = "redrawn".into(),
                        Key::Char('j') | Key::Down | Key::PageDown => {
                            let step = if matches!(key, Key::PageDown) { 10 } else { 1 };
                            let height = body_height(super::terminal::RawMode::size().0);
                            screen.scroll_by(step, height, super::terminal::RawMode::size().1 as usize);
                        }
                        Key::Char('k') | Key::Up | Key::PageUp => {
                            let step = if matches!(key, Key::PageUp) { 10 } else { 1 };
                            let height = body_height(super::terminal::RawMode::size().0);
                            screen.scroll_by(-step, height, super::terminal::RawMode::size().1 as usize);
                        }
                        Key::Char('g') => screen.follow = false,
                        Key::Char('G') => {
                            screen.jump_to_end(
                                body_height(super::terminal::RawMode::size().0),
                                super::terminal::RawMode::size().1 as usize,
                            );
                        }
                        Key::Backspace => {
                            input.pop();
                        }
                        Key::Char(c) => input.push(c),
                        Key::Ignored => {}
                    }
                }
            }
            message = messages.recv() => {
                let Some(message) = message else { break; };
                if let Some(event) = projected_event(&message) {
                    track(
                        &event,
                        message.get("params").and_then(|p| p.get("turn_id")).and_then(Value::as_str),
                        &mut active_turn,
                        &mut queued,
                        &mut pending_approval,
                        &mut status,
                    );
                    screen.push(event_line(&event));
                }
            }
            _ = resize.recv() => {
                // Resize: the next loop iteration redraws with the new size.
            }
        }
    }
    drop(raw);
    Ok(())
}

fn body_height(rows: u16) -> usize {
    rows.saturating_sub(3).max(1) as usize
}

async fn open_session(
    requester: &mut super::jsonl_client::JsonlRequester,
    messages: &mut tokio::sync::mpsc::Receiver<Value>,
    options: &super::tui::TuiOptions,
) -> Result<(SessionId, Vec<Value>), String> {
    if let Some(session_id) = options.resume_session {
        let response = requester
            .request(
                "session.resume",
                json!({
                    "event_log": options.event_log.clone().unwrap_or_else(|| {
                std::env::temp_dir().join(format!("mini-harness-tui-{}.jsonl", std::process::id()))
            }),
                    "workspace": options.workspace,
                    "session_id": session_id,
                    "replay_events": true,
                }),
            )
            .await?;
        let mut history = Vec::new();
        if let Some(events) = response["result"]["events"].as_array() {
            history.extend(events.iter().cloned());
        }
        drain_pending(messages, &mut history).await;
        Ok((session_id, history))
    } else {
        let mut params = json!({
            "event_log": options.event_log.clone().unwrap_or_else(|| {
                std::env::temp_dir().join(format!("mini-harness-tui-{}.jsonl", std::process::id()))
            }),
            "workspace": options.workspace,
            "provider": options.provider,
        });
        if let Some(model) = &options.model {
            params["model"] = json!(model);
        }
        let response = requester.request("session.create", params).await?;
        let session_id = response["result"]["session_id"]
            .as_str()
            .and_then(|id| SessionId::parse(id).ok())
            .ok_or_else(|| "session.create did not return a session id".to_owned())?;
        drain_pending(messages, &mut Vec::new()).await;
        Ok((session_id, Vec::new()))
    }
}

async fn drain_pending(
    messages: &mut tokio::sync::mpsc::Receiver<Value>,
    history: &mut Vec<Value>,
) {
    while let Ok(message) = messages.try_recv() {
        history.push(message);
    }
}

#[allow(clippy::too_many_arguments)]
fn track(
    event: &Value,
    turn_id: Option<&str>,
    active_turn: &mut Option<String>,
    queued: &mut usize,
    pending_approval: &mut Option<String>,
    status: &mut String,
) {
    let kind = event["type"].as_str().unwrap_or_default();
    match kind {
        "turn.started" => {
            *active_turn = turn_id.map(str::to_owned);
            *status = "RunningTurn".into();
        }
        "tool.approval.requested" => {
            *status = "WaitingApproval".into();
            let call = event["tool_call_id"].as_str().unwrap_or_default();
            let turn = turn_id.unwrap_or_default();
            *pending_approval = Some(format!("{turn}|{call}"));
        }
        "tool.approval.responded" | "tool.policy_denied" => {
            *pending_approval = None;
        }
        "tool.started" => *status = "RunningTool".into(),
        "user.input.recorded" => {
            if active_turn.is_some() {
                *queued += 1;
            }
        }
        "turn.completed" | "turn.failed" | "turn.cancelled" | "turn.timed_out" => {
            *active_turn = None;
            *pending_approval = None;
            *queued = 0;
            *status = "Idle".into();
        }
        _ => {}
    }
}

fn decode_approval(value: &str) -> Option<(String, String)> {
    let (turn, call) = value.split_once('|')?;
    Some((turn.to_owned(), call.to_owned()))
}

async fn submit_line(
    requester: &mut super::jsonl_client::JsonlRequester,
    session: &SessionId,
    line: &str,
    active_turn: &mut Option<String>,
    queued: &mut usize,
    status: &mut String,
) -> Result<(), String> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(());
    }
    let response = requester
        .request("turn.start", json!({"session_id": session, "prompt": line}))
        .await?;
    if response.get("error").is_some_and(|e| !e.is_null()) {
        return Err(response["error"]["message"]
            .as_str()
            .unwrap_or("turn.start failed")
            .to_owned());
    }
    let turn = response["result"]["turn_id"].as_str().map(str::to_owned);
    if response["result"]["queued"] == Value::Bool(true) {
        *queued += 1;
        *status = "queued".into();
    } else {
        *active_turn = turn;
        *status = "RunningTurn".into();
    }
    Ok(())
}

fn projected_event(message: &Value) -> Option<Value> {
    if message.get("method").and_then(Value::as_str) == Some("event") {
        message
            .get("params")
            .and_then(|params| params.get("event"))
            .cloned()
    } else {
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Key {
    Char(char),
    Backspace,
    Up,
    Down,
    PageUp,
    PageDown,
    Ignored,
}

/// Decodes key presses from the raw byte stream, carrying an escape buffer
/// and a partial-UTF-8 buffer across chunked reads.
///
/// Two subtleties this exists for: multi-byte input (CJK must arrive as
/// `Char`s, not be swallowed) and a *lone* ESC — when the next byte is not
/// `[`, the ESC itself is noise but the following key must survive.
fn keys(bytes: &[u8], escape: &mut Vec<u8>, utf8: &mut Vec<u8>) -> Vec<Key> {
    let mut keys = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if !utf8.is_empty() {
            utf8.push(byte);
            index += 1;
            match std::str::from_utf8(utf8) {
                Ok(text) => {
                    if let Some(character) = text.chars().next() {
                        keys.push(Key::Char(character));
                    }
                    utf8.clear();
                }
                Err(error) if error.error_len().is_none() => {
                    // Incomplete sequence: wait for the next chunk.
                }
                Err(_) => {
                    keys.push(Key::Ignored);
                    utf8.clear();
                }
            }
            continue;
        }
        if !escape.is_empty() {
            if escape.as_slice() == [0x1b] && byte != b'[' {
                // A lone ESC: drop it and reprocess this byte as a normal
                // key (see the test `lone_escape_does_not_swallow_the_next_key`).
                escape.clear();
                keys.push(Key::Ignored);
                continue;
            }
            escape.push(byte);
            // A CSI sequence ends at its final byte ('A'..'~'); '[' itself is
            // always an intermediate byte.
            if (0x40..=0x7e).contains(&byte) && byte != b'[' || escape.len() >= 8 {
                keys.push(match escape.as_slice() {
                    b"\x1b[A" => Key::Up,
                    b"\x1b[B" => Key::Down,
                    b"\x1b[5~" => Key::PageUp,
                    b"\x1b[6~" => Key::PageDown,
                    _ => Key::Ignored,
                });
                escape.clear();
            }
            index += 1;
            continue;
        }
        match byte {
            0x1b => escape.push(byte),
            b'\r' | b'\n' => keys.push(Key::Char('\n')),
            0x7f | 0x08 => keys.push(Key::Backspace),
            0x03 => keys.push(Key::Char('\x03')),
            0x04 => keys.push(Key::Char('\x04')),
            0x20..=0x7e => keys.push(Key::Char(byte as char)),
            0x80..=0xbf => keys.push(Key::Ignored), // stray continuation byte
            _ => utf8.push(byte),                   // UTF-8 lead byte
        }
        index += 1;
    }
    keys
}

#[allow(clippy::too_many_arguments)]
/// Builds one full-screen frame.
///
/// Raw mode disables `OPOST`, so `\n` moves the cursor down **without**
/// returning to column 0 — a bare newline would draw a staircase. Every line
/// break in a frame must therefore be an explicit `\r\n`.
fn build_frame(header: &str, body: &[String], input: &str) -> String {
    let mut out = String::from("\x1b[H\x1b[2J");
    out.push_str(header);
    out.push_str("\r\n");
    for line in body {
        out.push_str(line);
        out.push_str("\r\n");
    }
    out.push_str("\x1b[K> ");
    out.push_str(input);
    out.push_str("\x1b[K");
    out
}

#[allow(clippy::too_many_arguments)]
fn draw(
    rows: u16,
    cols: u16,
    screen: &Screen,
    session: &SessionId,
    status: &str,
    queued: usize,
    approval: Option<&str>,
    input: &str,
) {
    let body = {
        let mut screen = screen.clone_view();
        screen.render(cols as usize, body_height(rows))
    };
    let body = body
        .iter()
        .map(|line| truncate_width(line, cols as usize))
        .collect::<Vec<_>>();
    let frame = build_frame(
        &header(session, status, queued, approval, cols as usize),
        &body,
        input,
    );
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(frame.as_bytes());
    let _ = stdout.flush();
}

impl Screen {
    fn clone_view(&self) -> Screen {
        Screen {
            lines: self.lines.clone(),
            scroll: self.scroll,
            follow: self.follow,
            folded: self.folded,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_event(kind: &str) -> Value {
        json!({"type": kind})
    }

    #[test]
    fn wraps_double_width_characters_by_cell() {
        let rows = wrap_line("中文abc", 4);
        // 中 and 文 are two cells each and fit exactly one width-4 row; the
        // ascii tail wraps to the next row. A width-3 cut splits the pair.
        assert_eq!(rows, vec!["中文".to_owned(), "abc".to_owned()]);
        assert_eq!(rows[0].width(), 4);
        let tight = wrap_line("中文", 3);
        assert_eq!(tight.len(), 2, "{tight:?}");
    }

    #[test]
    fn folds_verbose_lines_and_toggles() {
        let long = format!("tool.completed  result={}", "x".repeat(400));
        let event = json!({"type": "tool.completed", "result": long});
        let body = event_line(&event);
        assert!(body.full.is_some());
        let mut screen = Screen {
            folded: true,
            follow: true,
            ..Screen::default()
        };
        screen.push(body);
        let folded_rows = screen.render(80, 10);
        assert!(
            folded_rows.iter().any(|row| row.contains("+f")),
            "{folded_rows:?}"
        );
        let folded_len: usize = folded_rows.iter().map(String::len).sum();
        screen.folded = false;
        let full_rows = screen.render(80, 10);
        let full_len: usize = full_rows.iter().map(String::len).sum();
        assert!(full_len > folded_len, "unfolding must reveal more text");
    }

    #[test]
    fn tracks_lifecycle_from_event_projections() {
        let mut active = None;
        let mut queued = 0;
        let mut approval = None;
        let mut status = String::new();
        track(
            &line_event("turn.started"),
            Some("turn_1"),
            &mut active,
            &mut queued,
            &mut approval,
            &mut status,
        );
        assert_eq!(status, "RunningTurn");
        track(
            &json!({"type": "tool.approval.requested", "tool_call_id": "call_1"}),
            Some("turn_1"),
            &mut active,
            &mut queued,
            &mut approval,
            &mut status,
        );
        assert_eq!(status, "WaitingApproval");
        assert!(approval.is_some());
        track(
            &line_event("turn.cancelled"),
            Some("turn_1"),
            &mut active,
            &mut queued,
            &mut approval,
            &mut status,
        );
        assert_eq!(status, "Idle");
        assert!(approval.is_none());
    }

    #[test]
    fn decodes_utf8_input_as_characters() {
        // CJK input arrives as multi-byte UTF-8 and must become Char keys,
        // not be silently swallowed.
        let mut escape = Vec::new();
        let mut utf8 = Vec::new();
        let decoded = keys("你好".as_bytes(), &mut escape, &mut utf8);
        assert_eq!(decoded, vec![Key::Char('你'), Key::Char('好')]);

        // Mixed ascii + CJK in one read chunk.
        let mut escape = Vec::new();
        let mut utf8 = Vec::new();
        let decoded = keys("a中b".as_bytes(), &mut escape, &mut utf8);
        assert_eq!(
            decoded,
            vec![Key::Char('a'), Key::Char('中'), Key::Char('b')]
        );

        // A sequence split across chunked reads still decodes.
        let mut escape = Vec::new();
        let mut utf8 = Vec::new();
        assert!(keys("你".as_bytes()[..2].as_ref(), &mut escape, &mut utf8).is_empty());
        let decoded = keys(&"你".as_bytes()[2..], &mut escape, &mut utf8);
        assert_eq!(decoded, vec![Key::Char('你')]);
    }

    #[test]
    fn lone_escape_does_not_swallow_the_next_key() {
        // A bare ESC (not followed by '[') is Alt-ish noise; the *next*
        // regular key must survive.
        let mut escape = Vec::new();
        let mut bytes: Vec<u8> = vec![0x1b];
        bytes.extend_from_slice(b"a");
        let mut utf8 = Vec::new();
        let decoded = keys(&bytes, &mut escape, &mut utf8);
        assert_eq!(
            decoded,
            vec![Key::Ignored, Key::Char('a')],
            "ESC followed by 'a' must not eat the 'a'"
        );
    }

    #[test]
    fn frame_line_breaks_are_explicit_carriage_returns() {
        // Raw mode has OPOST off: a bare '\n' would staircase the display.
        let frame = build_frame("header", &["a".to_owned(), "b".to_owned()], "hi");
        assert!(frame.contains("header\r\na\r\nb\r\n"), "{frame:?}");
        let bare = frame
            .char_indices()
            .filter(|(index, c)| *c == '\n' && *index > 0 && frame.as_bytes()[index - 1] != b'\r')
            .count();
        assert_eq!(bare, 0, "every newline must be preceded by CR: {frame:?}");
    }

    #[test]
    fn decodes_arrow_and_page_keys() {
        let mut escape = Vec::new();
        let mut utf8 = Vec::new();
        let decoded = keys(b"\x1b[A\x1b[B", &mut escape, &mut utf8);
        assert_eq!(decoded, vec![Key::Up, Key::Down]);
        let mut escape = Vec::new();
        let mut utf8 = Vec::new();
        let decoded = keys(b"\x1b[5~", &mut escape, &mut utf8);
        assert_eq!(decoded, vec![Key::PageUp]);
    }
}
