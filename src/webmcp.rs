//! Client bindings for the `WebMCP` CDP domain.
//!
//! Some remote engines expose a non-standard, capability-noun `WebMCP`
//! domain (alongside standard protocol domains) that surfaces agent-callable
//! "tools" a page has declared, following the WebMCP tool model (`name`,
//! `description`, JSON-Schema `inputSchema`). These bindings are hand-written
//! [`Command`](chromey_types::Command) implementations issued as raw
//! method strings — no PDL definitions or code generation involved — mirroring
//! how other vendor methods (e.g. `Interception.setPolicy`) are sent.
//!
//! Only the standard tool descriptor fields are modeled as typed fields; any
//! additional server-specific metadata is preserved losslessly in
//! [`Tool::extra`] so the client stays neutral and forward-compatible.
//!
//! Browsers that do not implement the `WebMCP` domain respond with a regular
//! protocol error, surfaced through the crate's usual [`Result`](crate::error::Result).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// An agent-callable tool a page declared, as reported by `WebMCP.listTools`.
///
/// The typed fields cover the standard WebMCP tool descriptor. Anything else
/// the server includes is kept verbatim in [`Tool::extra`], so no data is
/// lost and future server additions do not break older clients.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    /// The tool's unique name, used to invoke it.
    pub name: String,
    /// Human/agent readable description of what the tool does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema describing the tool's arguments. Kept as raw JSON since it
    /// is an arbitrary schema object.
    #[serde(rename = "inputSchema", default)]
    pub input_schema: serde_json::Value,
    /// Target action associated with a declaratively-declared tool (e.g. a
    /// form action). Omitted on the wire when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    /// Whether the underlying action may be auto-submitted when the tool is
    /// invoked (declarative tools).
    #[serde(default)]
    pub autosubmit: bool,
    /// Any additional fields the server reported, preserved losslessly and
    /// opaquely (round-trips on re-serialization).
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// List the agent-callable tools the current page declared.
/// [listTools]: method `WebMCP.listTools`, takes no parameters.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ListToolsParams {}

impl ListToolsParams {
    pub const IDENTIFIER: &'static str = "WebMCP.listTools";
}

impl chromey_types::Method for ListToolsParams {
    fn identifier(&self) -> chromey_types::MethodId {
        Self::IDENTIFIER.into()
    }
}

impl chromey_types::MethodType for ListToolsParams {
    fn method_id() -> chromey_types::MethodId
    where
        Self: Sized,
    {
        Self::IDENTIFIER.into()
    }
}

/// The catalog of tools returned by `WebMCP.listTools`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ListToolsReturns {
    /// The tools the page declared.
    #[serde(default)]
    pub tools: Vec<Tool>,
}

impl chromey_types::Command for ListToolsParams {
    type Response = ListToolsReturns;
}

/// Invoke a page-declared tool by name.
/// [callTool]: method `WebMCP.callTool`.
///
/// An engine that implements `WebMCP.callTool` runs the tool and returns its
/// result as opaque JSON — commonly a WebMCP tool-result object with a
/// `content` array and an `isError` flag (a failing tool is reported in-band as
/// `isError: true`, not as a protocol error).
///
/// Not every engine implements `WebMCP.callTool`. Depending on the engine's
/// unknown-method policy, an unimplemented call surfaces either a protocol
/// error or an empty success result — treat an empty object as "not supported
/// yet" and discover live tools via `WebMCP.listTools`. The response is
/// returned as opaque JSON ([`serde_json::Value`]) so the client does not
/// constrain the result shape.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CallToolParams {
    /// The name of the tool to invoke, as reported by `WebMCP.listTools`.
    pub name: String,
    /// Arguments for the tool, matching its `inputSchema`.
    #[serde(default)]
    pub arguments: serde_json::Value,
}

impl CallToolParams {
    pub const IDENTIFIER: &'static str = "WebMCP.callTool";

    /// Create params for invoking `name` with the given `arguments`.
    pub fn new(name: impl Into<String>, arguments: serde_json::Value) -> Self {
        Self {
            name: name.into(),
            arguments,
        }
    }
}

impl chromey_types::Method for CallToolParams {
    fn identifier(&self) -> chromey_types::MethodId {
        Self::IDENTIFIER.into()
    }
}

impl chromey_types::MethodType for CallToolParams {
    fn method_id() -> chromey_types::MethodId
    where
        Self: Sized,
    {
        Self::IDENTIFIER.into()
    }
}

impl chromey_types::Command for CallToolParams {
    type Response = serde_json::Value;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SAMPLE: &str = r#"{ "tools": [ {
        "name": "createSupportRequest",
        "description": "File a support ticket",
        "inputSchema": { "type": "object",
          "properties": { "subject": { "type": "string", "description": "Short summary" },
                          "urgent": { "type": "boolean" },
                          "category": { "type": "string", "enum": ["billing","tech"] } },
          "required": ["subject"] },
        "source": "declarative",
        "action": "/support",
        "autosubmit": true,
        "integrity": { "trusted": false, "confidence": 0.7,
                       "hints": { "readOnly": false, "untrustedContent": true } }
    } ] }"#;

    #[test]
    fn deserializes_list_tools_sample_payload() {
        let returns: ListToolsReturns =
            serde_json::from_str(SAMPLE).expect("sample must deserialize");
        assert_eq!(returns.tools.len(), 1);
        let tool = &returns.tools[0];

        // Standard descriptor fields.
        assert_eq!(tool.name, "createSupportRequest");
        assert_eq!(tool.description.as_deref(), Some("File a support ticket"));
        assert_eq!(tool.action.as_deref(), Some("/support"));
        assert!(tool.autosubmit);

        // inputSchema stays an arbitrary JSON value and round-trips intact.
        let expected_schema = json!({
            "type": "object",
            "properties": {
                "subject": { "type": "string", "description": "Short summary" },
                "urgent": { "type": "boolean" },
                "category": { "type": "string", "enum": ["billing", "tech"] }
            },
            "required": ["subject"]
        });
        assert_eq!(tool.input_schema, expected_schema);

        // Server-specific metadata is not modeled: it lands opaquely in
        // `extra`, byte-for-byte.
        assert!(tool.extra.contains_key("integrity"));
        assert!(tool.extra.contains_key("source"));
        assert_eq!(tool.extra["source"], json!("declarative"));
        assert_eq!(
            tool.extra["integrity"],
            json!({ "trusted": false, "confidence": 0.7,
                    "hints": { "readOnly": false, "untrustedContent": true } })
        );

        // Lossless passthrough: re-serializing the tool reproduces the exact
        // wire object, extras included.
        let wire: serde_json::Value = serde_json::from_str(SAMPLE).expect("sample is valid json");
        let reserialized = serde_json::to_value(tool).expect("tool must serialize");
        assert_eq!(reserialized, wire["tools"][0]);
    }

    #[test]
    fn action_absent_deserializes_to_none() {
        let returns: ListToolsReturns = serde_json::from_value(json!({
            "tools": [{
                "name": "noAction",
                "description": "imperative tool",
                "inputSchema": { "type": "object" },
                "autosubmit": false
            }]
        }))
        .expect("tool without action must deserialize");
        let tool = &returns.tools[0];
        assert_eq!(tool.action, None);
        // An omitted action must also stay omitted when serialized back out.
        let reserialized = serde_json::to_value(tool).expect("tool must serialize");
        assert!(reserialized.get("action").is_none());
    }

    #[test]
    fn method_identifiers_match_the_domain() {
        use chromey_types::Method;

        assert_eq!(ListToolsParams::IDENTIFIER, "WebMCP.listTools");
        assert_eq!(CallToolParams::IDENTIFIER, "WebMCP.callTool");
        assert_eq!(ListToolsParams::default().identifier(), "WebMCP.listTools");
        assert_eq!(
            CallToolParams::new("t", json!({})).identifier(),
            "WebMCP.callTool"
        );
        // `listTools` takes no parameters — the params must serialize to `{}`.
        assert_eq!(
            serde_json::to_value(ListToolsParams::default()).expect("params must serialize"),
            json!({})
        );
    }

    #[test]
    fn tolerates_unknown_fields() {
        let returns: ListToolsReturns = serde_json::from_value(json!({
            "tools": [{
                "name": "future",
                "description": "from a newer server phase",
                "inputSchema": {},
                "futureField": 1
            }],
            "futureTopLevel": true
        }))
        .expect("unknown fields must be tolerated");
        let tool = &returns.tools[0];
        assert_eq!(tool.name, "future");
        assert_eq!(tool.extra["futureField"], json!(1));
    }
}
