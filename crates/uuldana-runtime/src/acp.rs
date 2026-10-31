//! Agent Client Protocol (ACP) client side.
//!
//! Uuldana is always the ACP Client. [`AcpClient`] builds the JSON-RPC lines it
//! sends and remembers the agent's pending permission requests.
//! [`AcpDecoder`] is the one generic ACP to [`AgentEvent`] mapping, used when an
//! adapter returns no decoder for the ACP transport.
//!
//! Transport framing (one JSON-RPC message per line) is handled by
//! `uuldana-transport`; this module only sees complete messages.

use std::collections::HashMap;
use std::fmt;

use serde_json::{Value, json};
use uuldana_core::*;

const NAMESPACE: &str = "acp";

/// Failures building a client message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcpError {
    /// The permission request is not pending.
    UnknownRequest(String),
    /// The agent offered no option matching the decision.
    NoMatchingOption(PermissionDecision),
    /// An automatic rule tried to allow an action. Only a user may allow.
    AutomaticAllow,
}

impl fmt::Display for AcpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownRequest(id) => write!(f, "no pending permission request {id}"),
            Self::NoMatchingOption(d) => write!(f, "agent offered no option for {d:?}"),
            Self::AutomaticAllow => f.write_str("only a user decision may allow an action"),
        }
    }
}

impl std::error::Error for AcpError {}

struct PendingPermission {
    options: Vec<(String, PermissionDecision)>,
}

/// Builds client messages and tracks agent requests that await an answer.
pub struct AcpClient {
    next_id: u64,
    versions: AcpVersionRange,
    negotiated: Option<u16>,
    pending: HashMap<String, PendingPermission>,
}

impl AcpClient {
    /// Creates a client that accepts the given protocol versions.
    pub fn new(versions: AcpVersionRange) -> Self {
        Self {
            next_id: 1,
            versions,
            negotiated: None,
            pending: HashMap::new(),
        }
    }

    /// The protocol version the agent confirmed, once `initialize` completed.
    pub fn negotiated_version(&self) -> Option<u16> {
        self.negotiated
    }

    fn request(&mut self, method: &str, params: Value) -> String {
        let id = self.next_id;
        self.next_id += 1;
        json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string()
    }

    /// The `initialize` request. Offers the highest supported version and
    /// advertises no optional client capability: Uuldana does not serve file or
    /// terminal requests, so the agent keeps using its own tools.
    pub fn initialize(&mut self) -> String {
        let params = json!({
            "protocolVersion": self.versions.max(),
            "clientCapabilities": {
                "fs": {"readTextFile": false, "writeTextFile": false},
                "terminal": false,
            },
            "clientInfo": {
                "name": "uuldana",
                "title": "Uuldana",
                "version": env!("CARGO_PKG_VERSION"),
            },
        });
        self.request("initialize", params)
    }

    /// `session/new` for an absolute workspace.
    pub fn new_session(&mut self, cwd: &std::path::Path) -> String {
        self.request(
            "session/new",
            json!({"cwd": cwd.to_string_lossy(), "mcpServers": []}),
        )
    }

    /// `session/load`. Valid only when the agent declared `loadSession`.
    pub fn load_session(&mut self, native: &NativeSessionId, cwd: &std::path::Path) -> String {
        self.request(
            "session/load",
            json!({"sessionId": native.as_str(), "cwd": cwd.to_string_lossy(), "mcpServers": []}),
        )
    }

    /// `session/prompt` with one text block.
    pub fn prompt(&mut self, native: &NativeSessionId, text: &str) -> String {
        self.request(
            "session/prompt",
            json!({"sessionId": native.as_str(), "prompt": [{"type": "text", "text": text}]}),
        )
    }

    /// `session/cancel` notification. Pending permission requests are answered
    /// with the `cancelled` outcome, as the protocol requires.
    pub fn cancel(&mut self, native: &NativeSessionId) -> Vec<String> {
        let mut lines = vec![
            json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": native.as_str()}})
                .to_string(),
        ];
        let mut ids: Vec<String> = self.pending.keys().cloned().collect();
        ids.sort();
        for id in ids {
            self.pending.remove(&id);
            lines.push(permission_reply(&id, json!({"outcome": "cancelled"})));
        }
        lines
    }

    /// Records state implied by one agent message: the negotiated version and
    /// pending permission requests. Malformed input is ignored; the decoder
    /// reports it.
    pub fn observe(&mut self, line: &str) {
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            return;
        };
        if let Some(version) = message
            .pointer("/result/protocolVersion")
            .and_then(Value::as_u64)
            .and_then(|v| u16::try_from(v).ok())
        {
            self.negotiated = negotiate_acp_version(self.versions, version).ok();
        }
        if message.get("method").and_then(Value::as_str) == Some("session/request_permission")
            && let Some(id) = message.get("id")
        {
            let options = message
                .pointer("/params/options")
                .and_then(Value::as_array)
                .map(|list| list.iter().filter_map(parse_option).collect())
                .unwrap_or_default();
            self.pending
                .insert(id.to_string(), PendingPermission { options });
        }
    }

    /// Answers a pending permission request. Automatic rules may only reject;
    /// allowing needs a user resolution (ADR 0004).
    pub fn respond_permission(
        &mut self,
        id: &PermissionRequestId,
        resolution: PermissionResolution,
    ) -> Result<String, AcpError> {
        let decision = resolution.decision();
        if decision.is_allow() && resolution.source() != DecisionSource::User {
            return Err(AcpError::AutomaticAllow);
        }
        let pending = self
            .pending
            .get(id.as_str())
            .ok_or_else(|| AcpError::UnknownRequest(id.as_str().to_owned()))?;
        let outcome = if decision == PermissionDecision::Cancelled {
            json!({"outcome": "cancelled"})
        } else {
            let option = pending
                .options
                .iter()
                .find(|(_, d)| *d == decision)
                .ok_or(AcpError::NoMatchingOption(decision))?;
            json!({"outcome": "selected", "optionId": option.0})
        };
        self.pending.remove(id.as_str());
        Ok(permission_reply(id.as_str(), outcome))
    }
}

fn permission_reply(raw_id: &str, outcome: Value) -> String {
    // The request id is kept as raw JSON so numeric ids are echoed as numbers.
    let id = serde_json::from_str::<Value>(raw_id).unwrap_or(Value::Null);
    json!({"jsonrpc": "2.0", "id": id, "result": {"outcome": outcome}}).to_string()
}

fn parse_option(option: &Value) -> Option<(String, PermissionDecision)> {
    let id = option.get("optionId")?.as_str()?.to_owned();
    let decision = match option.get("kind")?.as_str()? {
        "allow_once" => PermissionDecision::AllowOnce,
        "allow_always" => PermissionDecision::AllowAlways,
        "reject_once" => PermissionDecision::RejectOnce,
        "reject_always" => PermissionDecision::RejectAlways,
        _ => return None,
    };
    Some((id, decision))
}

/// Generic ACP to normalized event mapping. Stateless apart from the version
/// range it validates `initialize` against.
pub struct AcpDecoder {
    versions: AcpVersionRange,
}

impl AcpDecoder {
    /// Creates a decoder for the given accepted versions.
    pub fn new(versions: AcpVersionRange) -> Self {
        Self { versions }
    }

    fn message(&self, line: &str, out: &mut Vec<DecodedEvent>) {
        let value = match serde_json::from_str::<Value>(line) {
            Ok(value) if value.is_object() => value,
            _ => {
                out.push(DecodedEvent::synthetic(AgentEvent::ProviderError {
                    class: ErrorClass::ProtocolViolation,
                    message: "ACP message is not a JSON object".into(),
                }));
                return;
            }
        };
        let method = value.get("method").and_then(Value::as_str);
        match (method, value.get("id")) {
            (Some("session/update"), None) => self.update(&value, out),
            (Some("session/request_permission"), Some(id)) => permission(&value, id, out),
            (Some(other), _) => out.push(DecodedEvent::native(AgentEvent::Extension {
                namespace: NAMESPACE.into(),
                name: other.to_owned(),
                payload: value
                    .get("params")
                    .map(Value::to_string)
                    .unwrap_or_default(),
            })),
            (None, Some(_)) => self.response(&value, out),
            (None, None) => out.push(DecodedEvent::synthetic(AgentEvent::ProviderError {
                class: ErrorClass::ProtocolViolation,
                message: "ACP message has neither method nor id".into(),
            })),
        }
    }

    fn response(&self, value: &Value, out: &mut Vec<DecodedEvent>) {
        if let Some(error) = value.get("error") {
            let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
            let text = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("no message");
            let class = match code {
                -32000 => ErrorClass::AuthenticationRequired,
                -32601 => ErrorClass::Unsupported,
                -32700 | -32600 | -32602 => ErrorClass::ProtocolViolation,
                _ => ErrorClass::ProcessFailed,
            };
            out.push(DecodedEvent::native(AgentEvent::ProviderError {
                class,
                message: format!("ACP error {code}: {text}"),
            }));
            return;
        }
        let Some(result) = value.get("result") else {
            return;
        };
        if let Some(version) = result.get("protocolVersion").and_then(Value::as_u64) {
            let accepted = u16::try_from(version)
                .ok()
                .and_then(|v| negotiate_acp_version(self.versions, v).ok());
            match accepted {
                Some(_) => out.push(DecodedEvent::native(AgentEvent::Extension {
                    namespace: NAMESPACE.into(),
                    name: "initialized".into(),
                    payload: result.to_string(),
                })),
                None => out.push(DecodedEvent::native(AgentEvent::ProviderError {
                    class: ErrorClass::IncompatibleVersion,
                    message: format!(
                        "agent answered ACP version {version}, supported {}..={}",
                        self.versions.min(),
                        self.versions.max()
                    ),
                })),
            }
        }
        if let Some(id) = result
            .get("sessionId")
            .and_then(Value::as_str)
            .and_then(|s| NativeSessionId::new(s).ok())
        {
            out.push(DecodedEvent::native(AgentEvent::NativeSessionIdentified {
                native: id,
            }));
        }
        if let Some(reason) = result.get("stopReason").and_then(Value::as_str) {
            out.push(DecodedEvent::native(AgentEvent::TurnEnded {
                reason: stop_reason(reason),
            }));
        }
    }

    fn update(&self, value: &Value, out: &mut Vec<DecodedEvent>) {
        let Some(update) = value.pointer("/params/update") else {
            out.push(DecodedEvent::synthetic(AgentEvent::ProviderError {
                class: ErrorClass::ProtocolViolation,
                message: "session/update without an update".into(),
            }));
            return;
        };
        let kind = update
            .get("sessionUpdate")
            .and_then(Value::as_str)
            .unwrap_or("");
        let text = update
            .pointer("/content/text")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let event = match kind {
            "user_message_chunk" | "agent_message_chunk" if text.is_some() => {
                Some(AgentEvent::MessageChunk {
                    role: if kind == "user_message_chunk" {
                        Role::User
                    } else {
                        Role::Agent
                    },
                    text: text.unwrap_or_default(),
                })
            }
            "agent_thought_chunk" if text.is_some() => Some(AgentEvent::ThoughtChunk {
                text: text.unwrap_or_default(),
            }),
            "tool_call" => tool_id(update).map(|id| AgentEvent::ToolCall {
                id,
                kind: tool_kind(update.get("kind").and_then(Value::as_str)),
                title: update
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                status: tool_status(update.get("status").and_then(Value::as_str))
                    .unwrap_or(ToolStatus::Pending),
            }),
            "tool_call_update" => tool_id(update).map(|id| AgentEvent::ToolCallUpdate {
                id,
                status: tool_status(update.get("status").and_then(Value::as_str)),
                title: update
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            }),
            "usage_update" => Some(AgentEvent::UsageUpdated {
                context_used: update.get("used").and_then(Value::as_u64),
                context_size: update.get("size").and_then(Value::as_u64),
            }),
            _ => None,
        };
        out.push(DecodedEvent::native(event.unwrap_or_else(|| {
            AgentEvent::Extension {
                namespace: NAMESPACE.into(),
                name: if kind.is_empty() {
                    "session_update"
                } else {
                    kind
                }
                .to_owned(),
                payload: update.to_string(),
            }
        })));
    }
}

impl EventDecoder for AcpDecoder {
    fn decode(&mut self, frame: TransportFrame<'_>, out: &mut Vec<DecodedEvent>) {
        match frame {
            TransportFrame::Message(line) => self.message(line, out),
            TransportFrame::Stderr(bytes) => {
                let text = String::from_utf8_lossy(bytes);
                let text = text.trim();
                if !text.is_empty() {
                    out.push(DecodedEvent::inferred(AgentEvent::Diagnostic {
                        message: format!("agent stderr: {text}"),
                    }));
                }
            }
            TransportFrame::Exited(exit) => {
                out.push(DecodedEvent::native(AgentEvent::ProcessExited(exit)));
            }
            _ => {}
        }
    }
}

fn permission(value: &Value, id: &Value, out: &mut Vec<DecodedEvent>) {
    let Ok(request) = PermissionRequestId::new(id.to_string()) else {
        out.push(DecodedEvent::synthetic(AgentEvent::ProviderError {
            class: ErrorClass::ProtocolViolation,
            message: "permission request id is not representable".into(),
        }));
        return;
    };
    let options = value
        .pointer("/params/options")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(parse_option)
                .map(|(_, decision)| decision)
                .collect()
        })
        .unwrap_or_default();
    out.push(DecodedEvent::native(AgentEvent::PermissionRequested {
        id: request,
        tool_call: value.pointer("/params/toolCall").and_then(tool_id),
        options,
    }));
}

fn tool_id(value: &Value) -> Option<ToolCallId> {
    ToolCallId::new(value.get("toolCallId")?.as_str()?).ok()
}

fn stop_reason(reason: &str) -> StopReason {
    match reason {
        "end_turn" => StopReason::EndTurn,
        "max_tokens" => StopReason::MaxTokens,
        "max_turn_requests" => StopReason::MaxTurnRequests,
        "refusal" => StopReason::Refusal,
        "cancelled" => StopReason::Cancelled,
        _ => StopReason::Unknown,
    }
}

fn tool_kind(kind: Option<&str>) -> ToolKind {
    match kind {
        Some("read") => ToolKind::Read,
        Some("edit") => ToolKind::Edit,
        Some("delete") => ToolKind::Delete,
        Some("move") => ToolKind::Move,
        Some("search") => ToolKind::Search,
        Some("execute") => ToolKind::Execute,
        Some("think") => ToolKind::Think,
        Some("fetch") => ToolKind::Fetch,
        Some("switch_mode") => ToolKind::SwitchMode,
        _ => ToolKind::Other,
    }
}

fn tool_status(status: Option<&str>) -> Option<ToolStatus> {
    match status? {
        "pending" => Some(ToolStatus::Pending),
        "in_progress" => Some(ToolStatus::InProgress),
        "completed" => Some(ToolStatus::Completed),
        "failed" => Some(ToolStatus::Failed),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(line: &str) -> Vec<AgentEvent> {
        let mut decoder = AcpDecoder::new(AcpVersionRange::V1);
        let mut out = Vec::new();
        decoder.decode(TransportFrame::Message(line), &mut out);
        out.into_iter().map(|d| d.event).collect()
    }

    #[test]
    fn maps_message_and_thought_chunks() {
        let events = decode(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"halo"}}}}"#,
        );
        assert_eq!(
            events,
            vec![AgentEvent::MessageChunk {
                role: Role::Agent,
                text: "halo".into()
            }]
        );
        let events = decode(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"cek"}}}}"#,
        );
        assert_eq!(
            events,
            vec![AgentEvent::ThoughtChunk { text: "cek".into() }]
        );
    }

    #[test]
    fn maps_tool_calls_and_usage() {
        let events = decode(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"tool_call","toolCallId":"t1","title":"Baca file","kind":"read","status":"in_progress"}}}"#,
        );
        assert_eq!(
            events,
            vec![AgentEvent::ToolCall {
                id: ToolCallId::new("t1").unwrap(),
                kind: ToolKind::Read,
                title: "Baca file".into(),
                status: ToolStatus::InProgress,
            }]
        );
        let events = decode(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"usage_update","used":10,"size":200}}}"#,
        );
        assert_eq!(
            events,
            vec![AgentEvent::UsageUpdated {
                context_used: Some(10),
                context_size: Some(200)
            }]
        );
    }

    #[test]
    fn maps_responses_session_and_stop_reason() {
        assert_eq!(
            decode(r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"abc"}}"#),
            vec![AgentEvent::NativeSessionIdentified {
                native: NativeSessionId::new("abc").unwrap()
            }]
        );
        assert_eq!(
            decode(r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"cancelled"}}"#),
            vec![AgentEvent::TurnEnded {
                reason: StopReason::Cancelled
            }]
        );
    }

    #[test]
    fn rejects_incompatible_version_and_reports_errors() {
        let events = decode(r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":9}}"#);
        assert!(matches!(
            events.as_slice(),
            [AgentEvent::ProviderError {
                class: ErrorClass::IncompatibleVersion,
                ..
            }]
        ));
        let events =
            decode(r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"login"}}"#);
        assert!(matches!(
            events.as_slice(),
            [AgentEvent::ProviderError {
                class: ErrorClass::AuthenticationRequired,
                ..
            }]
        ));
    }

    #[test]
    fn malformed_data_is_a_protocol_violation_not_a_panic() {
        for line in [
            "not json",
            "[]",
            "{}",
            r#"{"method":"session/update","params":{}}"#,
        ] {
            let events = decode(line);
            assert!(
                matches!(
                    events.as_slice(),
                    [AgentEvent::ProviderError {
                        class: ErrorClass::ProtocolViolation,
                        ..
                    }]
                ),
                "{line}: {events:?}"
            );
        }
    }

    #[test]
    fn permission_roundtrip_never_allows_by_policy() {
        let request = r#"{"jsonrpc":"2.0","id":7,"method":"session/request_permission","params":{"sessionId":"s","toolCall":{"toolCallId":"t9"},"options":[{"optionId":"yes","name":"Izinkan","kind":"allow_once"},{"optionId":"no","name":"Tolak","kind":"reject_once"}]}}"#;
        let events = decode(request);
        let id = match &events[..] {
            [
                AgentEvent::PermissionRequested {
                    id,
                    tool_call,
                    options,
                },
            ] => {
                assert_eq!(tool_call.as_ref().unwrap().as_str(), "t9");
                assert_eq!(
                    options,
                    &[
                        PermissionDecision::AllowOnce,
                        PermissionDecision::RejectOnce
                    ]
                );
                id.clone()
            }
            other => panic!("unexpected {other:?}"),
        };
        let mut client = AcpClient::new(AcpVersionRange::V1);
        client.observe(request);
        // An automatic rule can only reject: constructing an allow is refused.
        assert!(PermissionResolution::by_policy(PermissionDecision::AllowOnce).is_err());
        let reject = PermissionResolution::by_policy(PermissionDecision::RejectOnce).unwrap();
        let reply: Value =
            serde_json::from_str(&client.respond_permission(&id, reject).unwrap()).unwrap();
        assert_eq!(reply["id"], 7);
        assert_eq!(reply["result"]["outcome"]["optionId"], "no");
        // The request is consumed.
        assert!(matches!(
            client.respond_permission(&id, reject),
            Err(AcpError::UnknownRequest(_))
        ));
    }

    #[test]
    fn user_allow_selects_the_matching_option_and_cancel_answers_pending() {
        let request = r#"{"jsonrpc":"2.0","id":"p1","method":"session/request_permission","params":{"options":[{"optionId":"ok","name":"Ya","kind":"allow_once"}]}}"#;
        let mut client = AcpClient::new(AcpVersionRange::V1);
        client.observe(request);
        let id = PermissionRequestId::new("\"p1\"").unwrap();
        let allow = PermissionResolution::by_user(PermissionDecision::AllowOnce);
        let reply: Value =
            serde_json::from_str(&client.respond_permission(&id, allow).unwrap()).unwrap();
        assert_eq!(reply["id"], "p1");
        assert_eq!(reply["result"]["outcome"]["optionId"], "ok");

        client.observe(request);
        let lines = client.cancel(&NativeSessionId::new("s").unwrap());
        assert_eq!(lines.len(), 2);
        let reply: Value = serde_json::from_str(&lines[1]).unwrap();
        assert_eq!(reply["result"]["outcome"]["outcome"], "cancelled");
    }

    #[test]
    fn client_requests_are_well_formed() {
        let mut client = AcpClient::new(AcpVersionRange::V1);
        let init: Value = serde_json::from_str(&client.initialize()).unwrap();
        assert_eq!(init["method"], "initialize");
        assert_eq!(init["params"]["protocolVersion"], 1);
        assert_eq!(init["params"]["clientCapabilities"]["terminal"], false);
        client.observe(r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}"#);
        assert_eq!(client.negotiated_version(), Some(1));
        let prompt: Value = serde_json::from_str(
            &client.prompt(&NativeSessionId::new("s").unwrap(), "perbaiki tes"),
        )
        .unwrap();
        assert_eq!(prompt["id"], 2);
        assert_eq!(prompt["params"]["prompt"][0]["text"], "perbaiki tes");
    }
}
