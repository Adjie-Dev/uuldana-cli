//! JSON form of normalized events, for the durable session journal.
//!
//! The core crate has no serialization dependency (ADR 0005), so the schema is
//! written out here. It is versioned by the journal header, never by the
//! events. Unknown event names decode to `None` and are skipped by the reader,
//! so an older Uuldana can still read a journal written by a newer one.

use serde_json::{Value, json};
use uuldana_core::*;

pub(crate) fn error_class(class: ErrorClass) -> String {
    match class {
        ErrorClass::Limit(kind) => format!("limit:{}", kind.as_str()),
        other => other.as_str().to_owned(),
    }
}

fn limit_kind(name: &str) -> Option<LimitKind> {
    Some(match name {
        "usage_quota" => LimitKind::UsageQuota,
        "rate_limit" => LimitKind::RateLimit,
        "context_window" => LimitKind::ContextWindow,
        "budget" => LimitKind::Budget,
        "unknown" => LimitKind::Unknown,
        _ => return None,
    })
}

fn parse_error_class(name: &str) -> Option<ErrorClass> {
    if let Some(kind) = name.strip_prefix("limit:") {
        return limit_kind(kind).map(ErrorClass::Limit);
    }
    Some(match name {
        "not_installed" => ErrorClass::NotInstalled,
        "incompatible_version" => ErrorClass::IncompatibleVersion,
        "authentication_required" => ErrorClass::AuthenticationRequired,
        "transient" => ErrorClass::Transient,
        "protocol_violation" => ErrorClass::ProtocolViolation,
        "unsupported" => ErrorClass::Unsupported,
        "permission_denied" => ErrorClass::PermissionDenied,
        "policy_violation" => ErrorClass::PolicyViolation,
        "process_failed" => ErrorClass::ProcessFailed,
        "cancelled" => ErrorClass::Cancelled,
        "configuration" => ErrorClass::Configuration,
        "internal" => ErrorClass::Internal,
        _ => return None,
    })
}

fn run_end(end: RunEnd) -> Value {
    match end {
        RunEnd::Completed => json!({"end": "completed"}),
        RunEnd::Exited { code } => json!({"end": "exited", "code": code}),
        RunEnd::LimitReached(kind) => json!({"end": "limit_reached", "limit": kind.as_str()}),
        RunEnd::Failed(class) => json!({"end": "failed", "class": error_class(class)}),
        RunEnd::Cancelled => json!({"end": "cancelled"}),
        RunEnd::Terminated => json!({"end": "terminated"}),
        _ => json!({"end": "unknown"}),
    }
}

fn parse_run_end(value: &Value) -> Option<RunEnd> {
    Some(match value.get("end")?.as_str()? {
        "completed" => RunEnd::Completed,
        "exited" => RunEnd::Exited {
            code: value
                .get("code")
                .and_then(Value::as_i64)
                .and_then(|c| i32::try_from(c).ok()),
        },
        "limit_reached" => RunEnd::LimitReached(limit_kind(value.get("limit")?.as_str()?)?),
        "failed" => RunEnd::Failed(parse_error_class(value.get("class")?.as_str()?)?),
        "cancelled" => RunEnd::Cancelled,
        "terminated" => RunEnd::Terminated,
        _ => return None,
    })
}

pub(crate) fn run_state(state: RunState) -> Value {
    match state {
        RunState::Pending => json!({"state": "pending"}),
        RunState::Launching => json!({"state": "launching"}),
        RunState::Running => json!({"state": "running"}),
        RunState::Stopping => json!({"state": "stopping"}),
        RunState::Ended(end) => json!({"state": "ended", "reason": run_end(end)}),
    }
}

pub(crate) fn parse_run_state(value: &Value) -> Option<RunState> {
    Some(match value.get("state")?.as_str()? {
        "pending" => RunState::Pending,
        "launching" => RunState::Launching,
        "running" => RunState::Running,
        "stopping" => RunState::Stopping,
        "ended" => RunState::Ended(parse_run_end(value.get("reason")?)?),
        _ => return None,
    })
}

fn link(link: NativeSessionLink) -> &'static str {
    match link {
        NativeSessionLink::New => "new",
        NativeSessionLink::Resumed => "resumed",
        NativeSessionLink::Forked => "forked",
        _ => "unspecified",
    }
}

pub(crate) fn parse_link(name: &str) -> NativeSessionLink {
    match name {
        "new" => NativeSessionLink::New,
        "resumed" => NativeSessionLink::Resumed,
        "forked" => NativeSessionLink::Forked,
        _ => NativeSessionLink::Unspecified,
    }
}

fn stop_reason(reason: StopReason) -> &'static str {
    match reason {
        StopReason::EndTurn => "end_turn",
        StopReason::MaxTokens => "max_tokens",
        StopReason::MaxTurnRequests => "max_turn_requests",
        StopReason::Refusal => "refusal",
        StopReason::Cancelled => "cancelled",
        StopReason::LimitReached => "limit_reached",
        _ => "unknown",
    }
}

fn parse_stop_reason(name: &str) -> StopReason {
    match name {
        "end_turn" => StopReason::EndTurn,
        "max_tokens" => StopReason::MaxTokens,
        "max_turn_requests" => StopReason::MaxTurnRequests,
        "refusal" => StopReason::Refusal,
        "cancelled" => StopReason::Cancelled,
        "limit_reached" => StopReason::LimitReached,
        _ => StopReason::Unknown,
    }
}

fn tool_kind(kind: ToolKind) -> &'static str {
    match kind {
        ToolKind::Read => "read",
        ToolKind::Edit => "edit",
        ToolKind::Delete => "delete",
        ToolKind::Move => "move",
        ToolKind::Search => "search",
        ToolKind::Execute => "execute",
        ToolKind::Think => "think",
        ToolKind::Fetch => "fetch",
        ToolKind::SwitchMode => "switch_mode",
        _ => "other",
    }
}

fn parse_tool_kind(name: &str) -> ToolKind {
    match name {
        "read" => ToolKind::Read,
        "edit" => ToolKind::Edit,
        "delete" => ToolKind::Delete,
        "move" => ToolKind::Move,
        "search" => ToolKind::Search,
        "execute" => ToolKind::Execute,
        "think" => ToolKind::Think,
        "fetch" => ToolKind::Fetch,
        "switch_mode" => ToolKind::SwitchMode,
        _ => ToolKind::Other,
    }
}

fn tool_status(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Pending => "pending",
        ToolStatus::InProgress => "in_progress",
        ToolStatus::Completed => "completed",
        ToolStatus::Failed => "failed",
    }
}

fn parse_tool_status(name: &str) -> Option<ToolStatus> {
    Some(match name {
        "pending" => ToolStatus::Pending,
        "in_progress" => ToolStatus::InProgress,
        "completed" => ToolStatus::Completed,
        "failed" => ToolStatus::Failed,
        _ => return None,
    })
}

fn decision(decision: PermissionDecision) -> &'static str {
    match decision {
        PermissionDecision::AllowOnce => "allow_once",
        PermissionDecision::AllowAlways => "allow_always",
        PermissionDecision::RejectOnce => "reject_once",
        PermissionDecision::RejectAlways => "reject_always",
        PermissionDecision::Cancelled => "cancelled",
    }
}

fn parse_decision(name: &str) -> Option<PermissionDecision> {
    Some(match name {
        "allow_once" => PermissionDecision::AllowOnce,
        "allow_always" => PermissionDecision::AllowAlways,
        "reject_once" => PermissionDecision::RejectOnce,
        "reject_always" => PermissionDecision::RejectAlways,
        "cancelled" => PermissionDecision::Cancelled,
        _ => return None,
    })
}

pub(crate) fn fidelity(fidelity: Fidelity) -> &'static str {
    match fidelity {
        Fidelity::Native => "native",
        Fidelity::Inferred => "inferred",
        Fidelity::Synthetic => "synthetic",
    }
}

pub(crate) fn parse_fidelity(name: &str) -> Option<Fidelity> {
    Some(match name {
        "native" => Fidelity::Native,
        "inferred" => Fidelity::Inferred,
        "synthetic" => Fidelity::Synthetic,
        _ => return None,
    })
}

/// Encodes an event as a JSON object with an `event` name.
pub fn encode_event(event: &AgentEvent) -> Value {
    match event {
        AgentEvent::RunStateChanged { from, to } => {
            json!({"event": "run_state_changed", "from": run_state(*from), "to": run_state(*to)})
        }
        AgentEvent::NativeSessionIdentified { native } => {
            json!({"event": "native_session_identified", "native": native.as_str()})
        }
        AgentEvent::NativeSessionChanged { native, link: l } => {
            json!({"event": "native_session_changed", "native": native.as_str(), "link": link(*l)})
        }
        AgentEvent::TurnStarted => json!({"event": "turn_started"}),
        AgentEvent::TurnEnded { reason } => {
            json!({"event": "turn_ended", "reason": stop_reason(*reason)})
        }
        AgentEvent::MessageChunk { role, text } => json!({
            "event": "message_chunk",
            "role": if *role == Role::User { "user" } else { "agent" },
            "text": text,
        }),
        AgentEvent::ThoughtChunk { text } => json!({"event": "thought_chunk", "text": text}),
        AgentEvent::ToolCall {
            id,
            kind,
            title,
            status,
        } => json!({
            "event": "tool_call",
            "id": id.as_str(),
            "kind": tool_kind(*kind),
            "title": title,
            "status": tool_status(*status),
        }),
        AgentEvent::ToolCallUpdate { id, status, title } => json!({
            "event": "tool_call_update",
            "id": id.as_str(),
            "status": status.map(tool_status),
            "title": title,
        }),
        AgentEvent::PermissionRequested {
            id,
            tool_call,
            options,
        } => json!({
            "event": "permission_requested",
            "id": id.as_str(),
            "tool_call": tool_call.as_ref().map(|t| t.as_str()),
            "options": options.iter().map(|d| decision(*d)).collect::<Vec<_>>(),
        }),
        AgentEvent::PermissionResolved { id, resolution } => json!({
            "event": "permission_resolved",
            "id": id.as_str(),
            "decision": decision(resolution.decision()),
            "source": match resolution.source() {
                DecisionSource::User => "user",
                DecisionSource::Policy => "policy",
                _ => "cancellation",
            },
        }),
        AgentEvent::UsageUpdated {
            context_used,
            context_size,
        } => json!({
            "event": "usage_updated",
            "used": context_used,
            "size": context_size,
        }),
        AgentEvent::LimitReached(signal) => json!({
            "event": "limit_reached",
            "kind": signal.kind.as_str(),
            "resets_at_ms": signal.resets_at.map(|t| t.unix_millis()),
            "message": signal.message,
        }),
        AgentEvent::ProviderError { class, message } => json!({
            "event": "provider_error",
            "class": error_class(*class),
            "message": message,
        }),
        AgentEvent::ProcessExited(exit) => json!({
            "event": "process_exited",
            "code": exit.code,
            "success": exit.success,
        }),
        AgentEvent::Diagnostic { message } => json!({"event": "diagnostic", "message": message}),
        AgentEvent::Extension {
            namespace,
            name,
            payload,
        } => json!({
            "event": "extension",
            "namespace": namespace,
            "name": name,
            "payload": payload,
        }),
        _ => json!({"event": "unknown"}),
    }
}

/// Decodes an event written by [`encode_event`]. Unknown or damaged records
/// return `None`.
pub fn decode_event(value: &Value) -> Option<AgentEvent> {
    let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
    let number = |key: &str| value.get(key).and_then(Value::as_u64);
    Some(match value.get("event")?.as_str()? {
        "run_state_changed" => AgentEvent::RunStateChanged {
            from: parse_run_state(value.get("from")?)?,
            to: parse_run_state(value.get("to")?)?,
        },
        "native_session_identified" => AgentEvent::NativeSessionIdentified {
            native: NativeSessionId::new(text("native")?).ok()?,
        },
        "native_session_changed" => AgentEvent::NativeSessionChanged {
            native: NativeSessionId::new(text("native")?).ok()?,
            link: parse_link(&text("link")?),
        },
        "turn_started" => AgentEvent::TurnStarted,
        "turn_ended" => AgentEvent::TurnEnded {
            reason: parse_stop_reason(&text("reason")?),
        },
        "message_chunk" => AgentEvent::MessageChunk {
            role: if text("role")? == "user" {
                Role::User
            } else {
                Role::Agent
            },
            text: text("text")?,
        },
        "thought_chunk" => AgentEvent::ThoughtChunk {
            text: text("text")?,
        },
        "tool_call" => AgentEvent::ToolCall {
            id: ToolCallId::new(text("id")?).ok()?,
            kind: parse_tool_kind(&text("kind")?),
            title: text("title")?,
            status: parse_tool_status(&text("status")?)?,
        },
        "tool_call_update" => AgentEvent::ToolCallUpdate {
            id: ToolCallId::new(text("id")?).ok()?,
            status: text("status").and_then(|s| parse_tool_status(&s)),
            title: text("title"),
        },
        "permission_requested" => AgentEvent::PermissionRequested {
            id: PermissionRequestId::new(text("id")?).ok()?,
            tool_call: text("tool_call").and_then(|t| ToolCallId::new(t).ok()),
            options: value
                .get("options")?
                .as_array()?
                .iter()
                .filter_map(|v| v.as_str().and_then(parse_decision))
                .collect(),
        },
        "permission_resolved" => {
            let decision = parse_decision(&text("decision")?)?;
            let resolution = match text("source")?.as_str() {
                "user" => PermissionResolution::by_user(decision),
                "policy" => PermissionResolution::by_policy(decision).ok()?,
                _ => PermissionResolution::cancelled(),
            };
            AgentEvent::PermissionResolved {
                id: PermissionRequestId::new(text("id")?).ok()?,
                resolution,
            }
        }
        "usage_updated" => AgentEvent::UsageUpdated {
            context_used: number("used"),
            context_size: number("size"),
        },
        "limit_reached" => AgentEvent::LimitReached(LimitSignal {
            kind: limit_kind(&text("kind")?)?,
            resets_at: number("resets_at_ms").map(Timestamp::from_unix_millis),
            message: text("message"),
        }),
        "provider_error" => AgentEvent::ProviderError {
            class: parse_error_class(&text("class")?)?,
            message: text("message")?,
        },
        "process_exited" => AgentEvent::ProcessExited(ExitInfo {
            code: value
                .get("code")
                .and_then(Value::as_i64)
                .and_then(|c| i32::try_from(c).ok()),
            success: value.get("success")?.as_bool()?,
        }),
        "diagnostic" => AgentEvent::Diagnostic {
            message: text("message")?,
        },
        "extension" => AgentEvent::Extension {
            namespace: text("namespace")?,
            name: text("name")?,
            payload: text("payload")?,
        },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_event() -> Vec<AgentEvent> {
        let tool = ToolCallId::new("call-1").unwrap();
        let request = PermissionRequestId::new("7").unwrap();
        vec![
            AgentEvent::RunStateChanged {
                from: RunState::Running,
                to: RunState::Ended(RunEnd::LimitReached(LimitKind::UsageQuota)),
            },
            AgentEvent::RunStateChanged {
                from: RunState::Stopping,
                to: RunState::Ended(RunEnd::Failed(ErrorClass::Limit(LimitKind::RateLimit))),
            },
            AgentEvent::RunStateChanged {
                from: RunState::Launching,
                to: RunState::Ended(RunEnd::Exited { code: Some(23) }),
            },
            AgentEvent::RunStateChanged {
                from: RunState::Pending,
                to: RunState::Launching,
            },
            AgentEvent::NativeSessionIdentified {
                native: NativeSessionId::new("abc").unwrap(),
            },
            AgentEvent::NativeSessionChanged {
                native: NativeSessionId::new("def").unwrap(),
                link: NativeSessionLink::Forked,
            },
            AgentEvent::TurnStarted,
            AgentEvent::TurnEnded {
                reason: StopReason::MaxTokens,
            },
            AgentEvent::MessageChunk {
                role: Role::User,
                text: "perbaiki tes login\nlalu jalankan lint".into(),
            },
            AgentEvent::ThoughtChunk {
                text: "memeriksa modul auth".into(),
            },
            AgentEvent::ToolCall {
                id: tool.clone(),
                kind: ToolKind::Execute,
                title: "cargo test".into(),
                status: ToolStatus::InProgress,
            },
            AgentEvent::ToolCallUpdate {
                id: tool.clone(),
                status: Some(ToolStatus::Failed),
                title: None,
            },
            AgentEvent::PermissionRequested {
                id: request.clone(),
                tool_call: Some(tool),
                options: vec![
                    PermissionDecision::AllowOnce,
                    PermissionDecision::RejectOnce,
                ],
            },
            AgentEvent::PermissionResolved {
                id: request.clone(),
                resolution: PermissionResolution::by_user(PermissionDecision::AllowOnce),
            },
            AgentEvent::PermissionResolved {
                id: request,
                resolution: PermissionResolution::by_policy(PermissionDecision::RejectOnce)
                    .unwrap(),
            },
            AgentEvent::UsageUpdated {
                context_used: Some(10),
                context_size: None,
            },
            AgentEvent::LimitReached(LimitSignal {
                kind: LimitKind::UsageQuota,
                resets_at: Some(Timestamp::from_unix_millis(1_700_000_000_000)),
                message: Some("batas pemakaian tercapai".into()),
            }),
            AgentEvent::ProviderError {
                class: ErrorClass::AuthenticationRequired,
                message: "perlu login".into(),
            },
            AgentEvent::ProcessExited(ExitInfo {
                code: None,
                success: false,
            }),
            AgentEvent::Diagnostic {
                message: "catatan".into(),
            },
            AgentEvent::Extension {
                namespace: "acp".into(),
                name: "plan".into(),
                payload: r#"{"a":1}"#.into(),
            },
        ]
    }

    #[test]
    fn every_event_round_trips() {
        for event in every_event() {
            let json = encode_event(&event);
            assert_eq!(decode_event(&json), Some(event.clone()), "{json}");
        }
    }

    #[test]
    fn unknown_and_damaged_records_decode_to_none() {
        for value in [
            json!({"event": "from_the_future"}),
            json!({"event": "turn_ended"}),
            json!({"no_event": 1}),
            json!({"event": "run_state_changed", "from": {"state": "pending"}}),
            json!("text"),
        ] {
            assert_eq!(decode_event(&value), None, "{value}");
        }
    }
}
