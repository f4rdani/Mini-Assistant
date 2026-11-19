pub mod tools;

use std::collections::HashMap;

use serde_json::Value;

use crate::deepseek::auth::Account;
use crate::deepseek::client::DeepSeekClient;
use crate::error::AppError;
use tools::{parse_tools, system_prompt};

pub struct Prepared {
    pub prompt: String,
    pub ref_file_ids: Vec<String>,
    pub thinking: bool,
    pub search: bool,
    pub model: String,
}

pub async fn prepare(
    body: &Value,
    ds: &DeepSeekClient,
    acc: &Account,
    default_model: &str,
) -> Result<Prepared, AppError> {
    let model = body
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or(default_model)
        .to_string();
    let lower = model.to_ascii_lowercase();

    let tool_specs = parse_tools(body.get("tools"));

    let messages = body
        .get("messages")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if messages.is_empty() {
        return Err(AppError::BadRequest("messages[] is required".into()));
    }

    let mut system: Vec<String> = Vec::new();
    let mut convo = String::new();
    let mut attachments: Vec<String> = Vec::new();
    // tool_call_id -> function name, so role:"tool" turns stay attributable
    let mut call_names: HashMap<String, String> = HashMap::new();

    for msg in &messages {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("user");
        let mut text = String::new();
        extract_parts(msg.get("content"), &mut text, &mut attachments);
        let text = text.trim().to_string();

        match role {
            "system" | "developer" => {
                if !text.is_empty() {
                    system.push(text);
                }
            }
            "assistant" => {
                let mut line = text.clone();
                if let Some(calls) = msg.get("tool_calls").and_then(|v| v.as_array()) {
                    for c in calls {
                        let name = c
                            .pointer("/function/name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("tool");
                        let args = c
                            .pointer("/function/arguments")
                            .and_then(|v| v.as_str())
                            .unwrap_or("{}");
                        if let Some(id) = c.get("id").and_then(|v| v.as_str()) {
                            call_names.insert(id.to_string(), name.to_string());
                        }
                        line.push_str(&format!(
                            "\n[assistant called tool `{name}` with arguments {args}]"
                        ));
                    }
                }
                if !line.trim().is_empty() {
                    convo.push_str(&format!("Assistant: {}\n\n", line.trim()));
                }
            }
            "tool" | "function" => {
                let call_id = msg.get("tool_call_id").and_then(|v| v.as_str());
                let name = msg
                    .get("name")
                    .and_then(|v| v.as_str())
                    .or_else(|| call_id.and_then(|id| call_names.get(id).map(|s| s.as_str())))
                    .unwrap_or("tool");
                match call_id {
                    Some(id) => {
                        convo.push_str(&format!("Tool result ({name}, id={id}): {text}\n\n"))
                    }
                    None => convo.push_str(&format!("Tool result ({name}): {text}\n\n")),
                }
            }
            _ => {
                if !text.is_empty() {
                    convo.push_str(&format!("User: {}\n\n", text));
                }
            }
        }
    }

    // tool schemas go into the system prompt
    if !tool_specs.is_empty() {
        system.push(system_prompt(&tool_specs));
    }

    let mut prompt = String::new();
    if !system.is_empty() {
        prompt.push_str(&system.join("\n\n"));
        prompt.push_str("\n\n");
    }
    prompt.push_str(convo.trim_end());

    // explicit file ids from the caller
    let mut ref_file_ids: Vec<String> = Vec::new();
    for key in ["ref_file_ids", "file_ids"] {
        if let Some(arr) = body.get(key).and_then(|v| v.as_array()) {
            for id in arr {
                if let Some(s) = id.as_str() {
                    ref_file_ids.push(s.to_string());
                }
            }
        }
    }

    // images / files from content parts -> upload
    for (idx, item) in attachments.iter().enumerate() {
        if let Some(id) = item.strip_prefix("id:") {
            ref_file_ids.push(id.to_string());
            continue;
        }
        let (mime, bytes) = ds
            .download(item)
            .await
            .map_err(|e| AppError::BadRequest(format!("attachment {idx}: {e}")))?;
        let filename = filename_for(&mime, idx);
        let id = ds
            .upload_file(acc, filename, mime.clone(), bytes)
            .await
            .map_err(AppError::from)?;
        ref_file_ids.push(id);
    }

    if prompt.trim().is_empty() {
        if ref_file_ids.is_empty() {
            return Err(AppError::BadRequest(
                "messages[] produced an empty prompt".into(),
            ));
        }
        // attachment-only turn (e.g. a single image)
        prompt.push_str("Please look at the attached file and respond.");
    }

    let thinking = body
        .get("thinking")
        .or_else(|| body.get("reasoning"))
        .and_then(|v| v.as_bool())
        .unwrap_or_else(|| {
            lower.contains("reason") || lower.contains("think") || lower.contains("r1")
        });

    let search = body
        .get("search")
        .or_else(|| body.get("web_search"))
        .and_then(|v| v.as_bool())
        .unwrap_or_else(|| lower.contains("search") || lower.contains("web"));

    Ok(Prepared {
        prompt,
        ref_file_ids,
        thinking,
        search,
        model,
    })
}

fn extract_parts(content: Option<&Value>, text: &mut String, files: &mut Vec<String>) {
    let Some(content) = content else { return };
    match content {
        Value::String(s) => text.push_str(s),
        Value::Array(parts) => {
            for part in parts {
                let ty = part.get("type").and_then(|v| v.as_str()).unwrap_or("text");
                match ty {
                    "text" | "input_text" => {
                        if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                            text.push_str(t);
                        }
                    }
                    "image_url" => {
                        let url = part
                            .pointer("/image_url/url")
                            .and_then(|v| v.as_str())
                            .or_else(|| part.get("image_url").and_then(|v| v.as_str()));
                        if let Some(u) = url {
                            files.push(u.to_string());
                        }
                    }
                    "file" | "input_file" => {
                        let f = part.get("file").unwrap_or(part);
                        if let Some(id) = f.get("file_id").and_then(|v| v.as_str()) {
                            files.push(format!("id:{id}"));
                        }
                        if let Some(d) = f.get("file_data").and_then(|v| v.as_str()) {
                            files.push(d.to_string());
                        }
                        if let Some(t) = f.get("text").and_then(|v| v.as_str()) {
                            text.push_str(t);
                        }
                    }
                    _ => {
                        if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                            text.push_str(t);
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

fn filename_for(mime: &str, idx: usize) -> String {
    let ext = match mime {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        "text/csv" => "csv",
        "application/json" => "json",
        _ => "bin",
    };
    format!("upload-{idx}.{ext}")
}
