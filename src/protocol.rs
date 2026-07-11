use std::collections::{BTreeMap, HashMap};

use serde_json::{Map, Value, json};

const DROPPED_REQUEST_FIELDS: &[&str] = &[
    "user",
    "service_tier",
    "metadata",
    "background",
    "prompt_cache_key",
    "temperature",
    "max_output_tokens",
    "stream_tool_calls",
    "x_grok_doom_loop_check",
    "x-grok-doom-loop-check",
];

const ALLOWED_REQUEST_FIELDS: &[&str] = &[
    "model",
    "input",
    "instructions",
    "tools",
    "tool_choice",
    "parallel_tool_calls",
    "reasoning",
    "include",
    "max_tool_calls",
    "text",
    "truncation",
    "stream",
    "store",
];

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("request body must be a JSON object")]
    RootMustBeObject,
    #[error("request must include a non-empty model")]
    MissingModel,
    #[error("unsupported model: {0}; only gpt-5.6-sol and gpt-5.6-terra are supported")]
    UnsupportedModel(String),
    #[error("request input must be a string or an array")]
    InvalidInput,
    #[error("request instructions must be a string")]
    InvalidInstructions,
    #[error("unsupported request field: {0}")]
    UnsupportedField(String),
}

#[derive(Default)]
pub struct StreamTransformer {
    items_by_index: BTreeMap<usize, Value>,
    id_to_index: HashMap<String, usize>,
    call_metadata: HashMap<String, CallMetadata>,
}

#[derive(Clone, Debug, Default)]
struct CallMetadata {
    name: Option<String>,
    call_id: Option<String>,
}

pub fn adapt_request(request: Value) -> Result<Value, ProtocolError> {
    let mut request = request
        .as_object()
        .cloned()
        .ok_or(ProtocolError::RootMustBeObject)?;

    for field in DROPPED_REQUEST_FIELDS {
        request.remove(*field);
    }
    for field in request.keys() {
        if !ALLOWED_REQUEST_FIELDS.contains(&field.as_str()) {
            return Err(ProtocolError::UnsupportedField(field.clone()));
        }
    }

    let model = request
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
        .ok_or(ProtocolError::MissingModel)?
        .to_owned();
    if !matches!(model.as_str(), "gpt-5.6-sol" | "gpt-5.6-terra") {
        return Err(ProtocolError::UnsupportedModel(model));
    }
    request.insert("model".to_owned(), Value::String(model));
    request.insert("stream".to_owned(), Value::Bool(true));
    request.insert("store".to_owned(), Value::Bool(false));

    let mut instructions = request
        .remove("instructions")
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or(ProtocolError::InvalidInstructions)
        })
        .transpose()?
        .into_iter()
        .collect::<Vec<_>>();

    let input = request
        .remove("input")
        .unwrap_or_else(|| Value::Array(vec![]));
    let input = normalize_input(input, &mut instructions)?;
    request.insert("input".to_owned(), Value::Array(input));
    request.insert(
        "instructions".to_owned(),
        Value::String(if instructions.is_empty() {
            "You are a helpful coding assistant.".to_owned()
        } else {
            instructions.join("\n\n")
        }),
    );

    let reasoning = request.remove("reasoning");
    request.insert("reasoning".to_owned(), normalize_reasoning(reasoning));
    let include = request.remove("include");
    request.insert("include".to_owned(), normalize_include(include));

    if !request.contains_key("parallel_tool_calls") {
        // Preserve the caller's decision for standard Responses models.
        request.remove("parallel_tool_calls");
    }

    Ok(Value::Object(request))
}

fn normalize_input(
    input: Value,
    instructions: &mut Vec<String>,
) -> Result<Vec<Value>, ProtocolError> {
    let items = match input {
        Value::String(text) => vec![json!({
            "role": "user",
            "content": [{"type": "input_text", "text": text}],
        })],
        Value::Array(items) => items,
        _ => return Err(ProtocolError::InvalidInput),
    };

    let mut output = Vec::with_capacity(items.len());
    for item in items {
        let Some(mut item) = item.as_object().cloned() else {
            output.push(item);
            continue;
        };
        let role = item.get("role").and_then(Value::as_str).unwrap_or_default();
        if matches!(role, "system" | "developer") {
            if let Some(text) = extract_text(item.get("content"))
                && !text.is_empty()
            {
                instructions.push(text);
            }
            continue;
        }
        if matches!(role, "user" | "assistant")
            && let Some(content) = item.get("content").cloned()
        {
            item.insert(
                "content".to_owned(),
                normalize_message_content(role, content),
            );
        }
        if item.get("type").and_then(Value::as_str) == Some("reasoning")
            && !replayable_reasoning(&item)
        {
            continue;
        }
        normalize_history_item(&mut item);
        output.push(Value::Object(item));
    }
    Ok(output)
}

fn replayable_reasoning(item: &Map<String, Value>) -> bool {
    // Codex resolves a replayed reasoning item from its ciphertext; with
    // `store: false` an id-only item triggers a server-side lookup that always
    // fails. Grok also occasionally re-synthesizes history with fabricated
    // UUID-style ids (upstream ids are dash-free hex) whose ciphertext no
    // longer verifies. Either way Codex rejects the whole request, so only
    // items that kept their original id and ciphertext may be replayed.
    let has_ciphertext = item
        .get("encrypted_content")
        .and_then(Value::as_str)
        .is_some_and(|content| !content.is_empty());
    let has_upstream_id = item
        .get("id")
        .and_then(Value::as_str)
        .is_some_and(|id| id.starts_with("rs_") && !id.contains('-'));
    has_ciphertext && has_upstream_id
}

fn normalize_message_content(role: &str, content: Value) -> Value {
    let expected_type = if role == "user" {
        "input_text"
    } else {
        "output_text"
    };
    match content {
        Value::String(text) => Value::Array(vec![json!({"type": expected_type, "text": text})]),
        Value::Array(parts) => Value::Array(
            parts
                .into_iter()
                .map(|part| match part {
                    Value::String(text) => json!({"type": expected_type, "text": text}),
                    Value::Object(mut part) => {
                        if part.get("type").and_then(Value::as_str) == Some("text") {
                            part.insert("type".to_owned(), Value::String(expected_type.to_owned()));
                        }
                        Value::Object(part)
                    }
                    other => other,
                })
                .collect(),
        ),
        other => other,
    }
}

fn normalize_history_item(item: &mut Map<String, Value>) {
    match item.get("type").and_then(Value::as_str) {
        Some("reasoning") => {
            item.entry("summary".to_owned())
                .or_insert_with(|| Value::Array(vec![]));
        }
        Some("function_call") => {
            if item.get("id").is_none()
                && let Some(call_id) = item.get("call_id").and_then(Value::as_str)
            {
                item.insert("id".to_owned(), Value::String(format!("fc_{call_id}")));
            }
            item.entry("status".to_owned())
                .or_insert_with(|| Value::String("completed".to_owned()));
            item.entry("arguments".to_owned())
                .or_insert_with(|| Value::String(String::new()));
        }
        Some("function_call_output") => {
            item.entry("output".to_owned())
                .or_insert_with(|| Value::String(String::new()));
        }
        _ => {}
    }
}

fn normalize_reasoning(reasoning: Option<Value>) -> Value {
    let effort = reasoning
        .as_ref()
        .and_then(|value| value.get("effort"))
        .and_then(Value::as_str)
        .map_or("medium", map_effort);
    json!({"effort": effort, "summary": "concise"})
}

fn map_effort(effort: &str) -> &str {
    match effort.to_lowercase().as_str() {
        "none" | "off" => "none",
        "minimal" | "light" | "low" => "low",
        "medium" | "med" => "medium",
        "high" => "high",
        "extra_high" | "extrahigh" | "extra-high" | "xhigh" | "ultra" => "xhigh",
        "max" | "maximum" => "max",
        _ => "medium",
    }
}

fn normalize_include(include: Option<Value>) -> Value {
    let mut values = include
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default();
    if !values
        .iter()
        .any(|value| value.as_str() == Some("reasoning.encrypted_content"))
    {
        values.push(Value::String("reasoning.encrypted_content".to_owned()));
    }
    Value::Array(values)
}

fn extract_text(content: Option<&Value>) -> Option<String> {
    match content? {
        Value::String(text) => Some(text.to_owned()),
        Value::Array(parts) => Some(
            parts
                .iter()
                .filter_map(|part| match part {
                    Value::String(text) => Some(text.as_str()),
                    Value::Object(object) => object.get("text").and_then(Value::as_str),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        Value::Object(object) => object
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_owned),
        _ => None,
    }
}

impl StreamTransformer {
    pub fn transform(&mut self, mut payload: Value) -> Option<Value> {
        let event_type = payload.get("type").and_then(Value::as_str)?.to_owned();
        if matches!(
            event_type.as_str(),
            "keepalive"
                | "response.metadata"
                | "response.reasoning_text.delta"
                | "response.reasoning_text.done"
        ) {
            return None;
        }

        match event_type.as_str() {
            "response.output_item.added" | "response.output_item.done" => {
                self.transform_output_item(&mut payload, &event_type);
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_summary_text.done" => {
                self.attach_reasoning_index(&mut payload);
            }
            "response.function_call_arguments.delta" | "response.function_call_arguments.done" => {
                self.attach_function_metadata(&mut payload);
            }
            "response.completed" => self.patch_completed(&mut payload),
            _ => {}
        }
        Some(payload)
    }

    fn transform_output_item(&mut self, payload: &mut Value, event_type: &str) {
        let index = payload
            .get("output_index")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(0);
        let Some(item) = payload.get("item").and_then(Value::as_object).cloned() else {
            return;
        };
        let mut item = item;
        if let Some(id) = item.get("id").and_then(Value::as_str) {
            self.id_to_index.insert(id.to_owned(), index);
        }

        match item.get("type").and_then(Value::as_str) {
            Some("reasoning") => normalize_reasoning_item(&mut item),
            Some("function_call") => {
                item.entry("status".to_owned()).or_insert_with(|| {
                    Value::String(
                        if event_type == "response.output_item.done" {
                            "completed"
                        } else {
                            "in_progress"
                        }
                        .to_owned(),
                    )
                });
                item.entry("arguments".to_owned())
                    .or_insert_with(|| Value::String(String::new()));
                let metadata = CallMetadata {
                    name: item.get("name").and_then(Value::as_str).map(str::to_owned),
                    call_id: item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                };
                if let Some(id) = item.get("id").and_then(Value::as_str) {
                    self.call_metadata.insert(id.to_owned(), metadata.clone());
                }
                if let Some(call_id) = item.get("call_id").and_then(Value::as_str) {
                    self.call_metadata.insert(call_id.to_owned(), metadata);
                }
            }
            _ => {}
        }

        if event_type == "response.output_item.done" || !self.items_by_index.contains_key(&index) {
            self.items_by_index
                .insert(index, Value::Object(item.clone()));
        }
        if let Some(object) = payload.as_object_mut() {
            object.insert("item".to_owned(), Value::Object(item));
        }
    }

    fn attach_reasoning_index(&self, payload: &mut Value) {
        let Some(item_id) = payload.get("item_id").and_then(Value::as_str) else {
            return;
        };
        if payload.get("output_index").is_none()
            && let Some(index) = self.id_to_index.get(item_id)
        {
            payload["output_index"] = json!(index);
        }
    }

    fn attach_function_metadata(&self, payload: &mut Value) {
        let item_id = payload
            .get("item_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if payload.get("output_index").is_none()
            && let Some(index) = self.id_to_index.get(&item_id)
        {
            payload["output_index"] = json!(index);
        }
        if let Some(metadata) = self.call_metadata.get(&item_id) {
            if payload.get("call_id").is_none()
                && let Some(call_id) = &metadata.call_id
            {
                payload["call_id"] = json!(call_id);
            }
            if payload.get("name").is_none()
                && let Some(name) = &metadata.name
            {
                payload["name"] = json!(name);
            }
        }
    }

    fn patch_completed(&self, payload: &mut Value) {
        let Some(response) = payload.get_mut("response").and_then(Value::as_object_mut) else {
            return;
        };
        let current_output = response
            .get("output")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let normalized = current_output
            .iter()
            .cloned()
            .map(normalize_reasoning_value)
            .collect::<Vec<_>>();
        let has_reasoning = normalized
            .iter()
            .any(|item| item.get("type").and_then(Value::as_str) == Some("reasoning"));
        if current_output.is_empty() || has_reasoning {
            let output = if normalized.is_empty() {
                self.items_by_index.values().cloned().collect()
            } else {
                normalized
            };
            response.insert("output".to_owned(), Value::Array(output));
        }
    }
}

fn normalize_reasoning_value(value: Value) -> Value {
    let Some(mut object) = value.as_object().cloned() else {
        return value;
    };
    if object.get("type").and_then(Value::as_str) == Some("reasoning") {
        normalize_reasoning_item(&mut object);
    }
    Value::Object(object)
}

fn normalize_reasoning_item(item: &mut Map<String, Value>) {
    item.insert("content".to_owned(), Value::Array(vec![]));
    let summary = item
        .get("summary")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|part| match part {
            Value::String(text) => Some(json!({"type": "summary_text", "text": text})),
            Value::Object(object)
                if matches!(
                    object.get("type").and_then(Value::as_str),
                    None | Some("summary_text")
                ) =>
            {
                Some(json!({
                    "type": "summary_text",
                    "text": object.get("text").and_then(Value::as_str).unwrap_or_default(),
                }))
            }
            _ => None,
        })
        .collect();
    item.insert("summary".to_owned(), Value::Array(summary));
}

pub fn sse_data(payload: &Value) -> Vec<u8> {
    format!("data: {payload}\n\n").into_bytes()
}

pub fn failed_event(code: &str, message: &str) -> Value {
    json!({
        "type": "response.failed",
        "response": {
            "status": "failed",
            "error": {"code": code, "message": message},
        },
    })
}

pub fn usage_from_completed(payload: &Value) -> Option<ObservedUsage> {
    let usage = payload.pointer("/response/usage")?;
    let input_tokens = usage.get("input_tokens").and_then(Value::as_u64);
    let output_tokens = usage.get("output_tokens").and_then(Value::as_u64);
    let total_tokens = usage
        .get("total_tokens")
        .and_then(Value::as_u64)
        .or_else(|| {
            input_tokens
                .zip(output_tokens)
                .map(|(input, output)| input + output)
        });
    Some(ObservedUsage {
        input_tokens,
        output_tokens,
        total_tokens,
    })
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ObservedUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn adapts_messages_without_forcing_parallel_tools() {
        let request = json!({
            "model": "gpt-5.6-sol",
            "stream_tool_calls": true,
            "input": [
                {"role": "system", "content": "Be precise."},
                {"role": "user", "content": "Hello"}
            ],
            "reasoning": {"effort": "ultra"}
        });
        let adapted = adapt_request(request).expect("valid request");
        assert_eq!(adapted["instructions"], "Be precise.");
        assert_eq!(adapted["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(adapted["reasoning"]["effort"], "xhigh");
        assert!(adapted.get("stream_tool_calls").is_none());
        assert!(adapted.get("parallel_tool_calls").is_none());
    }

    #[test]
    fn drops_stream_events_unknown_to_grok() {
        let mut transformer = StreamTransformer::default();
        for event_type in ["keepalive", "response.metadata"] {
            assert!(
                transformer.transform(json!({"type": event_type})).is_none(),
                "{event_type} should not be forwarded"
            );
        }
    }

    #[test]
    fn replays_reasoning_history_with_intact_ciphertext() {
        let adapted = adapt_request(json!({
            "model": "gpt-5.6-sol",
            "input": [{
                "type": "reasoning",
                "id": "rs_0c5a6f856fc95898016a523ceadba481",
                "encrypted_content": "opaque-session-ciphertext",
                "summary": [{"type": "summary_text", "text": "Visible summary"}]
            }]
        }))
        .expect("request is valid");

        assert_eq!(
            adapted["input"][0]["encrypted_content"],
            "opaque-session-ciphertext"
        );
        assert_eq!(adapted["input"][0]["summary"][0]["text"], "Visible summary");
    }

    #[test]
    fn drops_unreplayable_reasoning_from_history() {
        let adapted = adapt_request(json!({
            "model": "gpt-5.6-sol",
            "input": [
                {
                    "type": "reasoning",
                    "id": "rs_3ee41ff4-f7fb-90ce-9631-f1ea72bdcad7",
                    "encrypted_content": "ciphertext-under-a-fabricated-id",
                    "summary": []
                },
                {
                    "type": "reasoning",
                    "id": "rs_0abe3732d6801efe016a522c6df35481",
                    "summary": []
                },
                {"role": "user", "content": "Hello"}
            ]
        }))
        .expect("request is valid");

        let input = adapted["input"].as_array().expect("input is an array");
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["role"], "user");
    }

    #[test]
    fn patches_completed_output_from_streamed_items() {
        let mut transformer = StreamTransformer::default();
        transformer.transform(json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {"type": "message", "id": "msg_1", "role": "assistant", "content": []}
        }));
        let completed = transformer
            .transform(json!({"type": "response.completed", "response": {"output": []}}))
            .expect("completion is preserved");
        assert_eq!(
            completed["response"]["output"].as_array().map(Vec::len),
            Some(1)
        );
    }

    #[test]
    fn enriches_function_deltas_for_the_tui() {
        let mut transformer = StreamTransformer::default();
        transformer.transform(json!({
            "type": "response.output_item.added",
            "output_index": 2,
            "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "ctx_tree"}
        }));
        let delta = transformer
            .transform(json!({"type": "response.function_call_arguments.delta", "item_id": "fc_1", "delta": "{}"}))
            .expect("delta is preserved");
        assert_eq!(delta["output_index"], 2);
        assert_eq!(delta["call_id"], "call_1");
        assert_eq!(delta["name"], "ctx_tree");
    }

    #[test]
    fn rejects_luna_and_unknown_models() {
        for model in ["gpt-5.6-luna", "gpt-5.6-unknown", "codex-sol"] {
            assert_eq!(
                adapt_request(json!({"model": model, "input": []})),
                Err(ProtocolError::UnsupportedModel(model.to_owned()))
            );
        }
    }

    #[test]
    fn drops_grok_sampling_fields_rejected_by_codex() {
        let adapted = adapt_request(json!({
            "model": "gpt-5.6-terra",
            "input": [],
            "temperature": 0.2,
            "max_output_tokens": 4_096
        }))
        .expect("Grok compaction request should adapt");

        assert_eq!(adapted.get("temperature"), None);
        assert_eq!(adapted.get("max_output_tokens"), None);
    }

    #[test]
    fn rejects_unknown_request_fields() {
        let error =
            adapt_request(json!({"model": "gpt-5.6-sol", "input": [], "secret_field": true}))
                .expect_err("unknown field is blocked");
        assert_eq!(
            error,
            ProtocolError::UnsupportedField("secret_field".to_owned())
        );
    }
}
