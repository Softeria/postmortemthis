//! Gemini-native -> OpenRouter (OpenAI chat/completions) translator. Lets the
//! gemini CLI run against OpenRouter, which exposes no Gemini-protocol
//! endpoint. Runs as the hidden `postmortemthis __gemshim` subcommand, spawned by
//! the launcher (see `gemshim.rs`) and configured via env. Buffered: calls
//! OpenRouter non-streaming and emits a single Gemini response (even for
//! :streamGenerateContent), so there is no streaming-delta reassembly.
use axum::{
    body::{Body, Bytes},
    extract::State,
    http::{Method, StatusCode},
    response::Response,
    routing::any,
    Router,
};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::Arc;

struct Cfg {
    client: reqwest::Client,
    key: String,
    model: String,
}

/// Entry point for the hidden subcommand: builds a runtime and serves until
/// killed. Config comes from OPENROUTER_API_KEY / GEMSHIM_PORT / GEMSHIM_MODEL.
pub fn run() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(serve());
}

async fn serve() {
    let cfg = Arc::new(Cfg {
        client: reqwest::Client::new(),
        key: std::env::var("OPENROUTER_API_KEY").expect("OPENROUTER_API_KEY"),
        model: std::env::var("GEMSHIM_MODEL").unwrap_or_else(|_| "google/gemini-3.1-pro-preview".into()),
    });
    let port: u16 = std::env::var("GEMSHIM_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8318);
    let app = Router::new().fallback(any(handle)).with_state(cfg);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.unwrap();
    eprintln!("gemshim: listening on 127.0.0.1:{port} -> OpenRouter");
    axum::serve(listener, app).await.unwrap();
}

async fn handle(State(cfg): State<Arc<Cfg>>, req: axum::extract::Request) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    // Model listing: gemini-cli may probe this. Return a minimal stub.
    if method == Method::GET && path.contains("/models") && !path.contains(':') {
        return jsonresp(StatusCode::OK, json!({
            "models": [{
                "name": "models/gemini-3.1-pro-preview",
                "supportedGenerationMethods": ["generateContent", "streamGenerateContent"]
            }]
        }));
    }

    let streaming = path.ends_with(":streamGenerateContent") || path.contains(":streamGenerateContent");
    if !(path.contains(":generateContent") || streaming) {
        return jsonresp(StatusCode::NOT_FOUND, json!({"error": {"message": format!("gemshim: unhandled {method} {path}")}}));
    }

    let body = match axum::body::to_bytes(req.into_body(), 64 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => return jsonresp(StatusCode::BAD_REQUEST, json!({"error":{"message": e.to_string()}})),
    };
    let gem: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return jsonresp(StatusCode::BAD_REQUEST, json!({"error":{"message": format!("bad gemini json: {e}")}})),
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
        Err(e) => return jsonresp(StatusCode::BAD_GATEWAY, json!({"error":{"message": format!("openrouter: {e}")}})),
    };
    let status = resp.status();
    let oai: Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => return jsonresp(StatusCode::BAD_GATEWAY, json!({"error":{"message": format!("openrouter body: {e}")}})),
    };
    if !status.is_success() {
        return jsonresp(StatusCode::BAD_GATEWAY, json!({"error":{"message": format!("openrouter {status}: {oai}")}}));
    }

    let gem_resp = openai_to_gemini(&oai);
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

fn jsonresp(status: StatusCode, v: Value) -> Response {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(Bytes::from(serde_json::to_vec(&v).unwrap())))
        .unwrap()
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

    // Gemini omits tool-call ids; OpenAI requires each tool result to carry
    // the id of the call it answers. Assign ids positionally and pair them
    // FIFO across the whole conversation, so multiple calls in one turn - even
    // to the same function - correlate correctly.
    let mut next_id: usize = 0;
    let mut pending: VecDeque<String> = VecDeque::new();

    for c in gem.get("contents").and_then(Value::as_array).into_iter().flatten() {
        let role = c.get("role").and_then(Value::as_str).unwrap_or("user");
        let parts = c.get("parts").and_then(Value::as_array).cloned().unwrap_or_default();

        // Collect text, function calls (model), function responses (user/tool).
        let mut text = String::new();
        let mut tool_calls: Vec<Value> = Vec::new();
        let mut tool_msgs: Vec<Value> = Vec::new();
        for p in &parts {
            if let Some(t) = p.get("text").and_then(Value::as_str) {
                text.push_str(t);
            } else if let Some(fc) = p.get("functionCall") {
                let name = fc.get("name").and_then(Value::as_str).unwrap_or("");
                let args = fc.get("args").cloned().unwrap_or(json!({}));
                let id = format!("call_{next_id}");
                next_id += 1;
                pending.push_back(id.clone());
                tool_calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": {"name": name, "arguments": serde_json::to_string(&args).unwrap_or("{}".into())}
                }));
            } else if let Some(fr) = p.get("functionResponse") {
                let response = fr.get("response").cloned().unwrap_or(json!({}));
                // Pair with the oldest unanswered call (FIFO); fall back to a
                // fresh id if a response arrives with no recorded call.
                let id = pending.pop_front().unwrap_or_else(|| {
                    let i = format!("call_{next_id}");
                    next_id += 1;
                    i
                });
                tool_msgs.push(json!({
                    "role": "tool",
                    "tool_call_id": id,
                    "content": serde_json::to_string(&response).unwrap_or("{}".into())
                }));
            }
        }

        if role == "model" {
            let mut m = json!({"role": "assistant"});
            if !text.is_empty() {
                m["content"] = json!(text);
            }
            if !tool_calls.is_empty() {
                m["tool_calls"] = json!(tool_calls);
            }
            // assistant message must have content or tool_calls
            if m.get("content").is_none() && m.get("tool_calls").is_none() {
                m["content"] = json!("");
            }
            messages.push(m);
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
            if let Some(p) = fd.get("parameters") {
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

fn openai_to_gemini(oai: &Value) -> Value {
    let choice = oai.get("choices").and_then(Value::as_array).and_then(|c| c.first());
    let msg = choice.and_then(|c| c.get("message"));
    let finish = choice.and_then(|c| c.get("finish_reason")).and_then(Value::as_str).unwrap_or("stop");

    let mut parts: Vec<Value> = Vec::new();
    if let Some(content) = msg.and_then(|m| m.get("content")).and_then(Value::as_str)
        && !content.is_empty()
    {
        parts.push(json!({"text": content}));
    }
    for tc in msg.and_then(|m| m.get("tool_calls")).and_then(Value::as_array).into_iter().flatten() {
        let f = tc.get("function");
        let name = f.and_then(|f| f.get("name")).and_then(Value::as_str).unwrap_or("");
        let args_str = f.and_then(|f| f.get("arguments")).and_then(Value::as_str).unwrap_or("{}");
        let args: Value = serde_json::from_str(args_str).unwrap_or(json!({}));
        parts.push(json!({"functionCall": {"name": name, "args": args}}));
    }
    if parts.is_empty() {
        parts.push(json!({"text": ""}));
    }

    let finish_reason = match finish {
        "length" => "MAX_TOKENS",
        _ => "STOP", // stop, tool_calls -> Gemini signals tool use via the part
    };

    let usage = oai.get("usage");
    let usage_meta = json!({
        "promptTokenCount": usage.and_then(|u| u.get("prompt_tokens")).cloned().unwrap_or(json!(0)),
        "candidatesTokenCount": usage.and_then(|u| u.get("completion_tokens")).cloned().unwrap_or(json!(0)),
        "totalTokenCount": usage.and_then(|u| u.get("total_tokens")).cloned().unwrap_or(json!(0)),
    });

    json!({
        "candidates": [{
            "content": {"role": "model", "parts": parts},
            "finishReason": finish_reason,
            "index": 0
        }],
        "usageMetadata": usage_meta
    })
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
        }));
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
        }));
        let parts = gem["candidates"][0]["content"]["parts"].as_array().unwrap();
        assert_eq!(parts[0]["functionCall"]["name"], "run_shell_command");
        assert_eq!(parts[0]["functionCall"]["args"]["command"], "git diff");
        assert_eq!(gem["candidates"][0]["finishReason"], "MAX_TOKENS");
    }
}
