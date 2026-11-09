//! Gemini-native -> OpenRouter (OpenAI chat/completions) translator. Lets the
//! Antigravity CLI (in its Gemini API key mode) run against OpenRouter, which
//! exposes no Gemini-protocol endpoint. Runs as the hidden `postmortemthis
//! __gemshim` subcommand, spawned by the launcher (see `gemshim.rs`).
//!
//! Contract with the launcher: config in env (OPENROUTER_API_KEY,
//! GEMSHIM_MODEL, GEMSHIM_SECRET); binds 127.0.0.1:0 and prints the port as the
//! first stdout line; exits when stdin hits EOF, so it dies with the launcher
//! however the launcher dies. Every request must carry GEMSHIM_SECRET as its
//! API key, so other local processes can't spend the OpenRouter credit.
//!
//! Buffered: calls OpenRouter non-streaming and emits a single Gemini response
//! (even for :streamGenerateContent), so there is no streaming-delta reassembly.
use axum::{
    Router,
    body::{Body, Bytes},
    extract::State,
    http::{Method, StatusCode},
    response::Response,
    routing::any,
};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::sync::Arc;

struct Cfg {
    client: reqwest::Client,
    key: String,
    model: String,
    secret: String,
}

/// Entry point for the hidden subcommand: serves until stdin closes.
pub fn run() {
    std::thread::spawn(|| {
        let _ = std::io::stdin().read_to_end(&mut Vec::new());
        std::process::exit(0);
    });
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(serve());
}

async fn serve() {
    let cfg = Arc::new(Cfg {
        client: reqwest::Client::new(),
        key: std::env::var("OPENROUTER_API_KEY").expect("OPENROUTER_API_KEY"),
        model: std::env::var("GEMSHIM_MODEL").expect("GEMSHIM_MODEL"),
        secret: std::env::var("GEMSHIM_SECRET").expect("GEMSHIM_SECRET"),
    });
    let app = Router::new().fallback(any(handle)).with_state(cfg);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.expect("bind loopback");
    let port = listener.local_addr().expect("local addr").port();
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{port}");
    let _ = out.flush();
    axum::serve(listener, app).await.expect("serve");
}

/// The key the client sent: the `x-goog-api-key` header, or `?key=`.
fn client_key(req: &axum::extract::Request) -> Option<String> {
    if let Some(k) = req.headers().get("x-goog-api-key").and_then(|v| v.to_str().ok()) {
        return Some(k.to_string());
    }
    req.uri()
        .query()?
        .split('&')
        .find_map(|kv| kv.strip_prefix("key="))
        .map(str::to_string)
}

async fn handle(State(cfg): State<Arc<Cfg>>, req: axum::extract::Request) -> Response {
    if client_key(&req).as_deref() != Some(cfg.secret.as_str()) {
        return gerr(StatusCode::UNAUTHORIZED, "gemshim: wrong or missing API key");
    }
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    // Model listing: the CLI may probe this. Return a minimal stub.
    if method == Method::GET && path.contains("/models") && !path.contains(':') {
        return jsonresp(
            StatusCode::OK,
            json!({
                "models": [{
                    "name": "models/gemini-3.1-pro-preview",
                    "supportedGenerationMethods": ["generateContent", "streamGenerateContent"]
                }]
            }),
        );
    }

    let streaming = path.contains(":streamGenerateContent");
    if method != Method::POST || !(path.contains(":generateContent") || streaming) {
        return gerr(StatusCode::NOT_FOUND, &format!("gemshim: unhandled {method} {path}"));
    }

    let body = match axum::body::to_bytes(req.into_body(), 64 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => return gerr(StatusCode::BAD_REQUEST, &e.to_string()),
    };
    let gem: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return gerr(StatusCode::BAD_REQUEST, &format!("bad gemini json: {e}")),
    };

    let oai_req = gemini_to_openai(&gem, &cfg.model);
    let upstream = cfg
        .client
        .post("https://openrouter.ai/api/v1/chat/completions")
        .bearer_auth(&cfg.key)
        .json(&oai_req)
        .send()
        .await;
    let resp = match upstream {
        Ok(r) => r,
        Err(e) => return gerr(StatusCode::SERVICE_UNAVAILABLE, &format!("openrouter: {e}")),
    };
    let status = resp.status();
    let oai: Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => return gerr(StatusCode::SERVICE_UNAVAILABLE, &format!("openrouter body: {e}")),
    };
    if !status.is_success() {
        return gerr(upstream_status(status.as_u16()), &format!("openrouter {status}: {oai}"));
    }
    // OpenRouter can report a provider failure inside a 200.
    if let Some(e) = oai.get("error") {
        let code = e.get("code").and_then(Value::as_u64).unwrap_or(502) as u16;
        return gerr(upstream_status(code), &format!("openrouter: {e}"));
    }

    let gem_resp = match openai_to_gemini(&oai) {
        Ok(v) => v,
        Err(why) => return gerr(StatusCode::SERVICE_UNAVAILABLE, &format!("gemshim: {why}")),
    };
    if streaming {
        // Emit the buffered response as a single SSE frame.
        let frame = format!("data: {}\n\n", serde_json::to_string(&gem_resp).unwrap());
        Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(Body::from(frame))
            .unwrap()
    } else {
        jsonresp(StatusCode::OK, gem_resp)
    }
}

/// The status to hand the client for an OpenRouter error. Antigravity retries
/// 429 and 5xx with backoff and gives up on the rest, so a bad key or an empty
/// balance must not come back as a 5xx, or the leg retries until it times out.
fn upstream_status(code: u16) -> StatusCode {
    match code {
        400 | 413 | 422 => StatusCode::BAD_REQUEST,
        401 => StatusCode::UNAUTHORIZED,
        // Out of credits: final, so not 429 (which the client would retry).
        402 | 403 => StatusCode::FORBIDDEN,
        404 => StatusCode::NOT_FOUND,
        429 => StatusCode::TOO_MANY_REQUESTS,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    }
}

/// An error in Google's shape, which the Gemini SDK reads its retry
/// decision from.
fn gerr(status: StatusCode, message: &str) -> Response {
    let grpc = match status {
        StatusCode::BAD_REQUEST => "INVALID_ARGUMENT",
        StatusCode::UNAUTHORIZED => "UNAUTHENTICATED",
        StatusCode::FORBIDDEN => "PERMISSION_DENIED",
        StatusCode::NOT_FOUND => "NOT_FOUND",
        StatusCode::TOO_MANY_REQUESTS => "RESOURCE_EXHAUSTED",
        _ => "UNAVAILABLE",
    };
    jsonresp(
        status,
        json!({"error": {"code": status.as_u16(), "message": message, "status": grpc}}),
    )
}

fn jsonresp(status: StatusCode, v: Value) -> Response {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(Bytes::from(serde_json::to_vec(&v).unwrap())))
        .unwrap()
}

/// OpenRouter's `reasoning_details` ride along as the Gemini `thoughtSignature`
/// on the turn's first part: the client echoes it back unchanged, and Gemini
/// models on OpenRouter want it back on the assistant turn for tool use.
fn encode_reasoning(details: &Value) -> String {
    B64.encode(serde_json::to_vec(details).unwrap_or_default())
}

fn decode_reasoning(sig: &str) -> Option<Value> {
    let v: Value = serde_json::from_slice(&B64.decode(sig).ok()?).ok()?;
    v.is_array().then_some(v)
}

fn gemini_to_openai(gem: &Value, model: &str) -> Value {
    let mut messages: Vec<Value> = Vec::new();

    // system_instruction / systemInstruction -> system message
    let sys = gem.get("systemInstruction").or_else(|| gem.get("system_instruction"));
    if let Some(s) = sys {
        let text = parts_text(s.get("parts"));
        if !text.is_empty() {
            messages.push(json!({"role": "system", "content": text}));
        }
    }

    // Tool-call ids: use the one the client sent (Antigravity echoes back the
    // id we put on each functionCall). Every call stays open until answered;
    // a response without an id answers the oldest open call, so a client that
    // omits ids still pairs multiple calls - even to the same function.
    let mut next_id: usize = 0;
    let mut open: Vec<String> = Vec::new();
    let fresh = |next_id: &mut usize| {
        let id = format!("call_{next_id}");
        *next_id += 1;
        id
    };

    for c in gem.get("contents").and_then(Value::as_array).into_iter().flatten() {
        let role = c.get("role").and_then(Value::as_str).unwrap_or("user");
        let parts = c.get("parts").and_then(Value::as_array).cloned().unwrap_or_default();

        // Sort parts by kind, not by the content's role: Antigravity sends its
        // functionResponse parts under role "model", and mapping that turn to
        // an assistant message leaves the request ending on a model turn,
        // which Gemini rejects.
        let mut text = String::new();
        let mut tool_calls: Vec<Value> = Vec::new();
        let mut tool_msgs: Vec<Value> = Vec::new();
        let mut reasoning: Option<Value> = None;
        for p in &parts {
            if reasoning.is_none() {
                reasoning = p.get("thoughtSignature").and_then(Value::as_str).and_then(decode_reasoning);
            }
            if let Some(t) = p.get("text").and_then(Value::as_str) {
                // Thought summaries are the model's own scratch, not the turn.
                if p.get("thought").and_then(Value::as_bool) != Some(true) {
                    text.push_str(t);
                }
            } else if let Some(fc) = p.get("functionCall") {
                let name = fc.get("name").and_then(Value::as_str).unwrap_or("");
                let args = fc.get("args").cloned().unwrap_or(json!({}));
                let id = match fc.get("id").and_then(Value::as_str) {
                    Some(id) => id.to_string(),
                    None => fresh(&mut next_id),
                };
                open.push(id.clone());
                tool_calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": {"name": name, "arguments": serde_json::to_string(&args).unwrap_or("{}".into())}
                }));
            } else if let Some(fr) = p.get("functionResponse") {
                let response = fr.get("response").cloned().unwrap_or(json!({}));
                let id = match fr.get("id").and_then(Value::as_str) {
                    Some(id) => {
                        open.retain(|o| o != id);
                        id.to_string()
                    }
                    None if !open.is_empty() => open.remove(0),
                    None => fresh(&mut next_id),
                };
                tool_msgs.push(json!({
                    "role": "tool",
                    "tool_call_id": id,
                    "content": serde_json::to_string(&response).unwrap_or("{}".into())
                }));
            }
        }

        if role == "model" {
            if !text.is_empty() || !tool_calls.is_empty() {
                let mut m = json!({"role": "assistant"});
                // OpenAI wants content present (null is fine) beside tool_calls.
                m["content"] = if text.is_empty() { Value::Null } else { json!(text) };
                if !tool_calls.is_empty() {
                    m["tool_calls"] = json!(tool_calls);
                }
                if let Some(r) = reasoning {
                    m["reasoning_details"] = r;
                }
                messages.push(m);
            }
            messages.extend(tool_msgs);
        } else {
            // Tool results must directly follow the assistant tool_calls turn,
            // so emit them before any trailing user text.
            messages.extend(tool_msgs);
            if !text.is_empty() {
                messages.push(json!({"role": "user", "content": text}));
            }
        }
    }

    let mut out = json!({"model": model, "messages": messages});

    // tools: functionDeclarations -> OpenAI tools
    let mut tools: Vec<Value> = Vec::new();
    for t in gem.get("tools").and_then(Value::as_array).into_iter().flatten() {
        for fd in t.get("functionDeclarations").and_then(Value::as_array).into_iter().flatten() {
            let mut func = json!({"name": fd.get("name").cloned().unwrap_or(json!(""))});
            if let Some(d) = fd.get("description") {
                func["description"] = d.clone();
            }
            // Antigravity sends plain JSON Schema as parametersJsonSchema;
            // older clients send the OpenAPI-subset `parameters`.
            if let Some(p) = fd.get("parametersJsonSchema").or_else(|| fd.get("parameters")) {
                func["parameters"] = p.clone();
            }
            tools.push(json!({"type": "function", "function": func}));
        }
    }
    if !tools.is_empty() {
        out["tools"] = json!(tools);
        out["tool_choice"] = json!("auto");
    }

    // generationConfig -> OpenAI params (subset)
    if let Some(gc) = gem.get("generationConfig") {
        if let Some(t) = gc.get("temperature") {
            out["temperature"] = t.clone();
        }
        if let Some(m) = gc.get("maxOutputTokens") {
            out["max_tokens"] = m.clone();
        }
        if let Some(p) = gc.get("topP") {
            out["top_p"] = p.clone();
        }
        if let Some(s) = gc.get("stopSequences") {
            out["stop"] = s.clone();
        }
    }

    out
}

/// The Gemini response for an OpenRouter completion, or why there is none
/// (the caller turns that into a retryable error).
fn openai_to_gemini(oai: &Value) -> Result<Value, String> {
    let choice = oai
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .ok_or("no choices in the OpenRouter reply")?;
    let msg = choice.get("message");
    let finish = choice.get("finish_reason").and_then(Value::as_str).unwrap_or("stop");
    if finish == "error" {
        return Err("the provider failed mid-reply (finish_reason error)".into());
    }

    let mut parts: Vec<Value> = Vec::new();
    if let Some(content) = msg.and_then(|m| m.get("content")).and_then(Value::as_str)
        && !content.is_empty()
    {
        parts.push(json!({"text": content}));
    }
    for tc in msg.and_then(|m| m.get("tool_calls")).and_then(Value::as_array).into_iter().flatten() {
        let f = tc.get("function");
        let name = f.and_then(|f| f.get("name")).and_then(Value::as_str).unwrap_or("");
        let args_str = f.and_then(|f| f.get("arguments")).and_then(Value::as_str).unwrap_or("");
        // An empty string is a call with no arguments; anything else must be
        // a JSON object. A truncated or garbled one is not run as `{}`.
        let args: Value = if args_str.trim().is_empty() {
            json!({})
        } else {
            match serde_json::from_str::<Value>(args_str) {
                Ok(v) if v.is_object() => v,
                _ => return Err(format!("the model sent malformed arguments for {name}")),
            }
        };
        let mut call = json!({"name": name, "args": args});
        // Hand the id to the client so its functionResponse can echo it back.
        if let Some(id) = tc.get("id").and_then(Value::as_str) {
            call["id"] = json!(id);
        }
        parts.push(json!({"functionCall": call}));
    }
    if parts.is_empty() {
        parts.push(json!({"text": ""}));
    }
    if let Some(details) = msg.and_then(|m| m.get("reasoning_details")).filter(|d| d.is_array())
        && let Some(first) = parts.first_mut()
    {
        first["thoughtSignature"] = json!(encode_reasoning(details));
    }

    let finish_reason = match finish {
        "length" => "MAX_TOKENS",
        "content_filter" => "SAFETY",
        _ => "STOP", // stop, tool_calls -> Gemini signals tool use via the part
    };

    let usage = oai.get("usage");
    let count = |k: &str| usage.and_then(|u| u.get(k)).and_then(Value::as_u64).unwrap_or(0);
    let usage_meta = json!({
        "promptTokenCount": count("prompt_tokens"),
        "candidatesTokenCount": count("completion_tokens"),
        "totalTokenCount": count("total_tokens"),
    });

    Ok(json!({
        "candidates": [{
            "content": {"role": "model", "parts": parts},
            "finishReason": finish_reason,
            "index": 0
        }],
        "usageMetadata": usage_meta
    }))
}

fn parts_text(parts: Option<&Value>) -> String {
    let mut s = String::new();
    for p in parts.and_then(Value::as_array).into_iter().flatten() {
        if let Some(t) = p.get("text").and_then(Value::as_str) {
            s.push_str(t);
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_roundtrip() {
        let oai = gemini_to_openai(&json!({"contents":[{"role":"user","parts":[{"text":"hi"}]}]}), "m");
        assert_eq!(oai["model"], "m");
        assert_eq!(oai["messages"][0]["role"], "user");
        assert_eq!(oai["messages"][0]["content"], "hi");

        let gem = openai_to_gemini(&json!({
            "choices":[{"message":{"content":"looks good"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":10,"completion_tokens":3,"total_tokens":13}
        })).unwrap();
        assert_eq!(gem["candidates"][0]["content"]["parts"][0]["text"], "looks good");
        assert_eq!(gem["candidates"][0]["finishReason"], "STOP");
        assert_eq!(gem["usageMetadata"]["promptTokenCount"], 10);
    }

    #[test]
    fn system_instruction_becomes_system_message() {
        let oai = gemini_to_openai(
            &json!({"systemInstruction":{"parts":[{"text":"be terse"}]},
                    "contents":[{"role":"user","parts":[{"text":"hi"}]}]}),
            "m",
        );
        assert_eq!(oai["messages"][0]["role"], "system");
        assert_eq!(oai["messages"][0]["content"], "be terse");
        assert_eq!(oai["messages"][1]["role"], "user");
    }

    #[test]
    fn function_declarations_become_tools() {
        let oai = gemini_to_openai(
            &json!({"contents":[],
                    "tools":[{"functionDeclarations":[
                        {"name":"run_shell_command","description":"run","parameters":{"type":"object"}}]}]}),
            "m",
        );
        assert_eq!(oai["tools"][0]["type"], "function");
        assert_eq!(oai["tools"][0]["function"]["name"], "run_shell_command");
        assert_eq!(oai["tool_choice"], "auto");
    }

    // The regression that name-based ids missed: two calls to the SAME function
    // in one turn must get distinct ids, and the results must pair in order.
    #[test]
    fn multiple_same_name_tool_calls_correlate_fifo() {
        let oai = gemini_to_openai(
            &json!({"contents":[
                {"role":"user","parts":[{"text":"check"}]},
                {"role":"model","parts":[
                    {"functionCall":{"name":"run_shell_command","args":{"command":"git diff"}}},
                    {"functionCall":{"name":"run_shell_command","args":{"command":"git log"}}}
                ]},
                {"role":"user","parts":[
                    {"functionResponse":{"name":"run_shell_command","response":{"output":"D"}}},
                    {"functionResponse":{"name":"run_shell_command","response":{"output":"L"}}}
                ]}
            ]}),
            "m",
        );
        let msgs = oai["messages"].as_array().unwrap();
        // [user, assistant(2 tool_calls), tool, tool]
        let tcs = msgs[1]["tool_calls"].as_array().unwrap();
        assert_eq!(tcs[0]["id"], "call_0");
        assert_eq!(tcs[1]["id"], "call_1");
        assert!(tcs[0]["function"]["arguments"].as_str().unwrap().contains("git diff"));
        assert_eq!(msgs[2]["role"], "tool");
        assert_eq!(msgs[2]["tool_call_id"], "call_0");
        assert_eq!(msgs[3]["tool_call_id"], "call_1");
        // distinct ids - the collision the old name-based scheme produced
        assert_ne!(msgs[2]["tool_call_id"], msgs[3]["tool_call_id"]);
    }

    #[test]
    fn response_tool_calls_and_finish_mapping() {
        let gem = openai_to_gemini(&json!({
            "choices":[{"message":{"tool_calls":[
                {"id":"x","type":"function","function":{"name":"run_shell_command","arguments":"{\"command\":\"git diff\"}"}}]},
                "finish_reason":"length"}]
        })).unwrap();
        let parts = gem["candidates"][0]["content"]["parts"].as_array().unwrap();
        assert_eq!(parts[0]["functionCall"]["name"], "run_shell_command");
        assert_eq!(parts[0]["functionCall"]["args"]["command"], "git diff");
        assert_eq!(gem["candidates"][0]["finishReason"], "MAX_TOKENS");
    }

    // Antigravity's shape: ids on both sides, and the functionResponse sent
    // under role "model". The request must still end on the tool result.
    #[test]
    fn antigravity_function_response_under_model_role() {
        let oai = gemini_to_openai(
            &json!({"contents":[
                {"role":"user","parts":[{"text":"read it"}]},
                {"role":"model","parts":[
                    {"text":"thinking...","thought":true},
                    {"functionCall":{"id":"abc","name":"read_file","args":{"Path":"x"}}}
                ]},
                {"role":"model","parts":[
                    {"functionResponse":{"id":"abc","name":"read_file","response":{"output":"X"}}}
                ]}
            ]}),
            "m",
        );
        let msgs = oai["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3, "{msgs:?}");
        assert_eq!(msgs[1]["role"], "assistant");
        assert!(msgs[1]["content"].is_null(), "thought text must not leak into the turn");
        assert_eq!(msgs[1]["tool_calls"][0]["id"], "abc");
        assert_eq!(msgs[2]["role"], "tool");
        assert_eq!(msgs[2]["tool_call_id"], "abc");
    }

    #[test]
    fn parameters_json_schema_becomes_tool_parameters() {
        let oai = gemini_to_openai(
            &json!({"contents":[],
                    "tools":[{"functionDeclarations":[
                        {"name":"read_file","parametersJsonSchema":{"type":"object","properties":{"Path":{"type":"string"}}}}]}]}),
            "m",
        );
        assert_eq!(oai["tools"][0]["function"]["parameters"]["properties"]["Path"]["type"], "string");
    }

    #[test]
    fn response_tool_call_carries_its_id() {
        let gem = openai_to_gemini(&json!({
            "choices":[{"message":{"tool_calls":[
                {"id":"tc1","type":"function","function":{"name":"read_file","arguments":"{}"}}]},
                "finish_reason":"tool_calls"}]
        })).unwrap();
        assert_eq!(gem["candidates"][0]["content"]["parts"][0]["functionCall"]["id"], "tc1");
    }

    #[test]
    fn response_without_id_answers_the_oldest_open_call_even_if_it_had_one() {
        let oai = gemini_to_openai(
            &json!({"contents":[
                {"role":"model","parts":[{"functionCall":{"id":"a","name":"f","args":{}}}]},
                {"role":"user","parts":[{"functionResponse":{"name":"f","response":{}}}]}
            ]}),
            "m",
        );
        assert_eq!(oai["messages"][1]["tool_call_id"], "a");
    }

    #[test]
    fn reasoning_details_roundtrip_through_thought_signature() {
        let details = json!([{"type":"reasoning.encrypted","data":"xyz"}]);
        let gem = openai_to_gemini(&json!({
            "choices":[{"message":{"content":null,"reasoning_details":details,"tool_calls":[
                {"id":"t","type":"function","function":{"name":"f","arguments":"{}"}}]},
                "finish_reason":"tool_calls"}]
        }))
        .unwrap();
        let part = gem["candidates"][0]["content"]["parts"][0].clone();
        assert!(part["thoughtSignature"].is_string());
        let oai = gemini_to_openai(&json!({"contents":[{"role":"model","parts":[part]}]}), "m");
        assert_eq!(oai["messages"][0]["reasoning_details"], details);
    }

    #[test]
    fn malformed_tool_arguments_are_an_error_not_an_empty_call() {
        let r = openai_to_gemini(&json!({
            "choices":[{"message":{"tool_calls":[
                {"id":"t","type":"function","function":{"name":"f","arguments":"{\"Path\": \"x"}}]},
                "finish_reason":"length"}]
        }));
        assert!(r.is_err());
    }

    #[test]
    fn error_finish_and_missing_choices_are_errors_and_filter_is_safety() {
        assert!(openai_to_gemini(&json!({"choices":[{"message":{"content":""},"finish_reason":"error"}]})).is_err());
        assert!(openai_to_gemini(&json!({"error":{"code":502}})).is_err());
        let gem = openai_to_gemini(&json!({
            "choices":[{"message":{"content":""},"finish_reason":"content_filter"}],
            "usage":{"prompt_tokens":null}
        }))
        .unwrap();
        assert_eq!(gem["candidates"][0]["finishReason"], "SAFETY");
        assert_eq!(gem["usageMetadata"]["promptTokenCount"], 0);
    }

    #[test]
    fn upstream_errors_keep_their_retry_meaning() {
        assert_eq!(upstream_status(401), StatusCode::UNAUTHORIZED);
        assert_eq!(upstream_status(402), StatusCode::FORBIDDEN);
        assert_eq!(upstream_status(429), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(upstream_status(500), StatusCode::SERVICE_UNAVAILABLE);
    }
}
