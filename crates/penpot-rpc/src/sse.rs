//! Minimal Server-Sent-Events parsing for `export-binfile` / `import-binfile`.
//!
//! Both commands answer `200` with `text/event-stream` (verified in M0):
//! `progress` events whose payloads are transit-encoded, then a final `end`
//! event. There is no direct binary response mode — the SSE dance is required.

use crate::error::{Error, Result};

/// One parsed SSE event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// Event type (`progress`, `end`, ... — defaults to `message` per spec).
    pub event: String,
    /// Data payload (multiple `data:` lines joined with `\n`).
    pub data: String,
}

/// Parse a complete `text/event-stream` body into events.
///
/// Handles CRLF and LF line endings, multiple `data:` lines per event, and
/// the optional single space after the field colon. Comment lines (leading
/// `:`) and unknown fields are ignored.
pub fn parse_sse(body: &str) -> Vec<SseEvent> {
    let mut events = Vec::new();
    let mut event_type: Option<String> = None;
    let mut data_lines: Vec<&str> = Vec::new();

    let mut dispatch = |event_type: &mut Option<String>, data_lines: &mut Vec<&str>| {
        if event_type.is_some() || !data_lines.is_empty() {
            events.push(SseEvent {
                event: event_type.take().unwrap_or_else(|| "message".to_string()),
                data: data_lines.join("\n"),
            });
            data_lines.clear();
        }
    };

    for raw_line in body.split('\n') {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if line.is_empty() {
            dispatch(&mut event_type, &mut data_lines);
            continue;
        }
        if line.starts_with(':') {
            continue; // comment
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "event" => event_type = Some(value.to_string()),
            "data" => data_lines.push(value),
            _ => {} // id, retry, unknown — irrelevant here
        }
    }
    // Stream may end without a trailing blank line.
    dispatch(&mut event_type, &mut data_lines);
    events
}

/// Find the `end` event in an SSE body, or fail with a protocol error.
pub fn find_end_event(body: &str) -> Result<SseEvent> {
    parse_sse(body)
        .into_iter()
        .find(|e| e.event == "end")
        .ok_or_else(|| Error::Protocol("SSE stream finished without an `end` event".into()))
}

/// Decode the `end` payload of `export-binfile`: a transit URI object,
/// e.g. `{"~#uri":"http://localhost:9001/assets/by-id/<uuid>"}`.
pub fn decode_export_end(data: &str) -> Result<String> {
    let value: serde_json::Value = serde_json::from_str(data)
        .map_err(|e| Error::Protocol(format!("export `end` event is not JSON: {e}; data={data}")))?;
    value
        .get("~#uri")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| {
            Error::Protocol(format!("export `end` event has no `~#uri` key: {data}"))
        })
}

/// Decode the `end` payload of `import-binfile`: the created file id(s),
/// transit-encoded. Penpot 2.16.2 answers in **two shapes** depending on the
/// source binfile format (both verified live in the N6 template spike):
/// - **array** form, from v3-zip imports:
///   `["~u3a4be581-6d37-8010-8008-51eecd7dc111"]`;
/// - **transit-set** form, from legacy binfile-v1 imports:
///   `{"~#set":["~u3a4be581-…"]}` — the ids live under the `~#set` key.
///
/// Both are accepted; each entry's `~u` transit-uuid prefix is stripped.
pub fn decode_import_end(data: &str) -> Result<Vec<String>> {
    let value: serde_json::Value = serde_json::from_str(data)
        .map_err(|e| Error::Protocol(format!("import `end` event is not JSON: {e}; data={data}")))?;
    // Accept the bare array, or the transit-set object `{"~#set":[…]}`.
    let arr = value
        .as_array()
        .or_else(|| value.get("~#set").and_then(|v| v.as_array()))
        .ok_or_else(|| {
            Error::Protocol(format!(
                "import `end` event is neither an array nor a `~#set` object: {data}"
            ))
        })?;
    arr.iter()
        .map(|v| {
            let s = v.as_str().ok_or_else(|| {
                Error::Protocol(format!("import `end` entry is not a string: {v}"))
            })?;
            // Transit-encoded uuid: `~u<uuid>`.
            Ok(s.strip_prefix("~u").unwrap_or(s).to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_progress_and_end_events_lf() {
        let body = "event: progress\ndata: {\"~:section\":\"~:file\"}\n\nevent: end\ndata: {\"~#uri\":\"http://x/assets/by-id/abc\"}\n\n";
        let events = parse_sse(body);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event, "progress");
        assert_eq!(events[0].data, "{\"~:section\":\"~:file\"}");
        assert_eq!(events[1].event, "end");
        assert_eq!(events[1].data, "{\"~#uri\":\"http://x/assets/by-id/abc\"}");
    }

    #[test]
    fn parses_crlf_and_missing_trailing_blank() {
        let body = "event: end\r\ndata: [\"~uAAAA\"]";
        let events = parse_sse(body);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "end");
        assert_eq!(events[0].data, "[\"~uAAAA\"]");
    }

    #[test]
    fn joins_multiple_data_lines_and_skips_comments() {
        let body = ": keepalive\ndata: line1\ndata: line2\n\n";
        let events = parse_sse(body);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "message");
        assert_eq!(events[0].data, "line1\nline2");
    }

    #[test]
    fn decode_export_end_extracts_transit_uri() {
        let uri =
            decode_export_end("{\"~#uri\":\"http://localhost:9001/assets/by-id/xyz\"}").unwrap();
        assert_eq!(uri, "http://localhost:9001/assets/by-id/xyz");
    }

    #[test]
    fn decode_import_end_strips_transit_uuid_prefix() {
        let ids =
            decode_import_end("[\"~u3a4be581-6d37-8010-8008-51eecd7dc111\"]").unwrap();
        assert_eq!(ids, vec!["3a4be581-6d37-8010-8008-51eecd7dc111".to_string()]);
    }

    #[test]
    fn decode_import_end_accepts_transit_set_form() {
        // Legacy binfile-v1 imports answer with the transit-SET shape
        // `{"~#set":["~u<id>"]}` (N6 spike). It must decode to the same ids.
        let ids =
            decode_import_end("{\"~#set\":[\"~u3a4be581-6d37-8010-8008-51eecd7dc111\"]}").unwrap();
        assert_eq!(ids, vec!["3a4be581-6d37-8010-8008-51eecd7dc111".to_string()]);
    }

    #[test]
    fn decode_import_end_rejects_unexpected_shape() {
        // A bare object with no `~#set` key is neither shape → protocol error.
        let err = decode_import_end("{\"oops\":1}").unwrap_err();
        assert!(matches!(err, Error::Protocol(_)));
    }

    #[test]
    fn missing_end_event_is_protocol_error() {
        let err = find_end_event("event: progress\ndata: {}\n\n").unwrap_err();
        assert!(matches!(err, Error::Protocol(_)));
    }
}
