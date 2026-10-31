//! Deterministic ACP agent used by integration tests. It speaks real JSON-RPC
//! over stdio, has no tools and never calls an AI service.
//!
//! Modes (first argument): `--version`, `default`, `permission`, `garbage`,
//! `old-version`.
use serde_json::{Value, json};
use std::io::{self, BufRead, Write};

fn send(value: Value) -> io::Result<()> {
    let mut out = io::stdout().lock();
    writeln!(out, "{value}")?;
    out.flush()
}

fn update(session: &str, body: Value) -> io::Result<()> {
    send(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {"sessionId": session, "update": body},
    }))
}

fn finish_turn(prompt_id: &Value, reason: &str) -> io::Result<()> {
    send(json!({"jsonrpc": "2.0", "id": prompt_id, "result": {"stopReason": reason}}))
}

fn main() -> io::Result<()> {
    let mode = std::env::args().nth(1).unwrap_or_default();
    if mode == "--version" {
        println!("uuldana-acp-fixture 0.1");
        return Ok(());
    }
    let session = "acp-fixture-1";
    // A prompt waiting for the client's permission answer: (prompt id, text).
    let mut awaiting: Option<(Value, String)> = None;
    for line in io::stdin().lock().lines() {
        let line = line?;
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = message.get("id").cloned();
        match (message.get("method").and_then(Value::as_str), id) {
            (Some("initialize"), Some(id)) => {
                if mode == "garbage" {
                    println!("this line is not json");
                    io::stdout().flush()?;
                }
                let version = if mode == "old-version" { 9 } else { 1 };
                send(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": version,
                        "agentCapabilities": {"loadSession": false},
                    },
                }))?;
            }
            (Some("session/new"), Some(id)) => {
                send(json!({"jsonrpc": "2.0", "id": id, "result": {"sessionId": session}}))?;
            }
            (Some("session/prompt"), Some(id)) => {
                let text = message
                    .pointer("/params/prompt/0/text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                update(
                    session,
                    json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "membaca permintaan"}}),
                )?;
                update(
                    session,
                    json!({"sessionUpdate": "tool_call", "toolCallId": "call-1", "title": "Baca README", "kind": "read", "status": "in_progress"}),
                )?;
                if mode == "permission" {
                    send(json!({
                        "jsonrpc": "2.0",
                        "id": 100,
                        "method": "session/request_permission",
                        "params": {
                            "sessionId": session,
                            "toolCall": {"toolCallId": "call-1"},
                            "options": [
                                {"optionId": "allow", "name": "Izinkan", "kind": "allow_once"},
                                {"optionId": "deny", "name": "Tolak", "kind": "reject_once"},
                            ],
                        },
                    }))?;
                    awaiting = Some((id, text));
                    continue;
                }
                complete(session, &id, &text)?;
            }
            (None, Some(id)) if id == 100 => {
                let Some((prompt_id, text)) = awaiting.take() else {
                    continue;
                };
                let outcome = message
                    .pointer("/result/outcome/outcome")
                    .and_then(Value::as_str);
                let option = message
                    .pointer("/result/outcome/optionId")
                    .and_then(Value::as_str);
                match (outcome, option) {
                    (Some("selected"), Some("allow")) => complete(session, &prompt_id, &text)?,
                    (Some("cancelled"), _) => finish_turn(&prompt_id, "cancelled")?,
                    _ => {
                        update(
                            session,
                            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "izin ditolak, langkah dilewati"}}),
                        )?;
                        finish_turn(&prompt_id, "end_turn")?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn complete(session: &str, prompt_id: &Value, text: &str) -> io::Result<()> {
    update(
        session,
        json!({"sessionUpdate": "tool_call_update", "toolCallId": "call-1", "status": "completed"}),
    )?;
    update(
        session,
        json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": format!("selesai: {text}")}}),
    )?;
    update(
        session,
        json!({"sessionUpdate": "usage_update", "used": 1200, "size": 200000}),
    )?;
    finish_turn(prompt_id, "end_turn")
}
