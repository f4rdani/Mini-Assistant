//! Function calling for a chat model that has no native tool support.
//!
//! We inject the JSON schemas into the system prompt and ask the model to emit
//! `<tool_call>{"name":..,"arguments":{..}}</tool_call>`. The stream parser below
//! strips those blocks out of the visible text and turns them into OpenAI
//! `tool_calls`.

use serde_json::{json, Value};

#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

pub fn parse_tools(tools: Option<&Value>) -> Vec<ToolSpec> {
    let Some(Value::Array(arr)) = tools else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for t in arr {
        // OpenAI: {type:"function", function:{name, description, parameters}}
        // Anthropic: {name, description, input_schema}
        let f = t.get("function").unwrap_or(t);
        let name = f
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if name.is_empty() {
            continue;
        }
        let description = f
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let parameters = f
            .get("parameters")
            .or_else(|| f.get("input_schema"))
            .cloned()
            .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
        out.push(ToolSpec {
            name,
            description,
            parameters,
        });
    }
    out
}

pub fn system_prompt(tools: &[ToolSpec]) -> String {
    let defs: Vec<Value> = tools
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "parameters": t.parameters,
            })
        })
        .collect();
    let defs = serde_json::to_string_pretty(&defs).unwrap_or_default();

    format!(
        r#"# Tool use

You can call the following external tools:

{defs}

## Calling convention

When you need a tool, reply with ONLY the tool call block(s) and nothing else:

<tool_call>{{"name": "tool_name", "arguments": {{"arg": "value"}}}}</tool_call>

Rules:
- `arguments` MUST be a single valid JSON object matching that tool's schema.
- Emit several <tool_call> blocks if several tools are needed in parallel.
- Never explain the <tool_call> syntax to the user.
- When a "Tool result" line is present, use it to answer the user in natural language.
- If no tool is needed, answer normally and never emit a <tool_call> block."#
    )
}

#[derive(Debug, Clone)]
pub struct ParsedCall {
    pub name: String,
    pub arguments: String,
}

const OPEN: &str = "<tool_call>";
const CLOSE: &str = "</tool_call>";

#[derive(Default)]
pub struct ToolParser {
    buf: String,
    in_call: bool,
    calls: Vec<ParsedCall>,
}

impl ToolParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn take_calls(&mut self) -> Vec<ParsedCall> {
        std::mem::take(&mut self.calls)
    }

    /// Feed a content delta, returns the text that should be shown to the user.
    pub fn feed(&mut self, delta: &str) -> String {
        self.buf.push_str(delta);
        let mut visible = String::new();

        loop {
            if self.in_call {
                if let Some(idx) = self.buf.find(CLOSE) {
                    let raw = self.buf[..idx].to_string();
                    self.buf.drain(..idx + CLOSE.len());
                    self.in_call = false;
                    if let Some(call) = parse_call(&raw) {
                        self.calls.push(call);
                    }
                    continue;
                }
                break;
            }

            if let Some(idx) = self.buf.find(OPEN) {
                visible.push_str(&self.buf[..idx]);
                self.buf.drain(..idx + OPEN.len());
                self.in_call = true;
                continue;
            }

            // hold back a partial marker at the tail
            let keep = partial_suffix_len(&self.buf, OPEN);
            let emit = self.buf.len() - keep;
            if emit > 0 {
                visible.push_str(&self.buf[..emit]);
                self.buf.drain(..emit);
            }
            break;
        }

        visible
    }

    /// Flush plain text that is still buffered. Returns "" while a call is open.
    pub fn finish(&mut self) -> String {
        if self.in_call {
            return String::new();
        }
        std::mem::take(&mut self.buf)
    }

    /// Drain an unterminated `<tool_call>` block. A block that still parses is
    /// promoted to a real call; otherwise the raw text is handed back so the
    /// caller can report it instead of silently dropping it.
    pub fn take_incomplete(&mut self) -> Option<String> {
        if !self.in_call {
            return None;
        }
        self.in_call = false;
        let raw = std::mem::take(&mut self.buf);
        if parse_call(&raw).is_some() {
            if let Some(call) = parse_call(&raw) {
                self.calls.push(call);
            }
            return None;
        }
        if raw.trim().is_empty() {
            None
        } else {
            Some(raw)
        }
    }
}

fn partial_suffix_len(buf: &str, marker: &str) -> usize {
    let b = buf.as_bytes();
    let m = marker.as_bytes();
    let max = m.len().min(b.len());
    // inclusive: the whole buffer may be a prefix of the marker ("<" on its own)
    for k in (1..=max).rev() {
        if b[b.len() - k..] == m[..k] {
            return k;
        }
    }
    0
}

fn parse_call(raw: &str) -> Option<ParsedCall> {
    let raw = raw.trim();
    let raw = raw
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    let value: Value = serde_json::from_str(raw).ok()?;

    let name = value
        .get("name")
        .or_else(|| value.pointer("/function/name"))
        .and_then(|v| v.as_str())?
        .to_string();

    let args = value
        .get("arguments")
        .or_else(|| value.get("parameters"))
        .or_else(|| value.get("input"))
        .cloned()
        .unwrap_or_else(|| json!({}));

    let arguments = match args {
        Value::String(s) => s,
        other => other.to_string(),
    };

    Some(ParsedCall { name, arguments })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_passes_through() {
        let mut p = ToolParser::new();
        assert_eq!(p.feed("hello "), "hello ");
        assert_eq!(p.feed("world"), "world");
        assert_eq!(p.finish(), "");
        assert!(p.take_calls().is_empty());
    }

    #[test]
    fn partial_marker_is_buffered() {
        let mut p = ToolParser::new();
        assert_eq!(p.feed("ok <tool_ca"), "ok ");
        assert_eq!(p.feed("ll>"), "");
        assert!(p.in_call);
    }

    #[test]
    fn single_tool_call_is_extracted() {
        let mut p = ToolParser::new();
        let out = p.feed("sure<tool_call>{\"name\":\"get_weather\",\"arguments\":{\"city\":\"Bandung\"}}</tool_call>");
        assert_eq!(out, "sure");
        let calls = p.take_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "get_weather");
        assert_eq!(calls[0].arguments, "{\"city\":\"Bandung\"}");
    }

    #[test]
    fn two_calls_in_one_chunk() {
        let mut p = ToolParser::new();
        p.feed("<tool_call>{\"name\":\"a\",\"arguments\":{}}</tool_call><tool_call>{\"name\":\"b\",\"arguments\":{\"x\":1}}</tool_call>");
        let calls = p.take_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].name, "b");
    }

    #[test]
    fn split_across_three_deltas() {
        let mut p = ToolParser::new();
        let mut visible = String::new();
        visible.push_str(&p.feed("Let me check. <tool_call>{\"na"));
        visible.push_str(&p.feed("me\":\"get_weather\",\"arguments\":{\"city\":\"Bandung\"}}"));
        visible.push_str(&p.feed("</tool_call>"));
        assert_eq!(visible, "Let me check. ");
        let calls = p.take_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "get_weather");
        assert_eq!(calls[0].arguments, "{\"city\":\"Bandung\"}");
    }

    #[test]
    fn char_by_char_tag_is_held_back() {
        // real stream chunks: "<", "tool", "_call", ">", then the json pieces
        let mut p = ToolParser::new();
        let mut visible = String::new();
        for chunk in ["<", "tool", "_call", ">", "{\"", "name", "\":", " \"", "get", "_", "weather"] {
            visible.push_str(&p.feed(chunk));
        }
        assert_eq!(visible, "", "tag must not leak to the client");
        assert!(p.in_call);
    }

    #[test]
    fn truncated_call_is_reported_not_dropped() {
        let mut p = ToolParser::new();
        p.feed("<tool_call>{\"name\":\"get_weather\",\"argum");
        assert!(p.take_calls().is_empty());
        let raw = p.take_incomplete().expect("truncated call must surface");
        assert!(raw.contains("get_weather"));
    }

    #[test]
    fn unterminated_but_valid_call_is_salvaged() {
        let mut p = ToolParser::new();
        p.feed("<tool_call>{\"name\":\"x\",\"arguments\":{}}");
        assert!(p.take_incomplete().is_none());
        assert_eq!(p.take_calls().len(), 1);
    }

    #[test]
    fn parses_openai_tool_schema() {
        let v = serde_json::json!([{
            "type": "function",
            "function": {"name": "f", "description": "d", "parameters": {"type": "object"}}
        }]);
        let specs = parse_tools(Some(&v));
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "f");
        assert!(!system_prompt(&specs).contains("get_weather"));
        assert!(system_prompt(&specs).contains("\"f\""));
    }
}
