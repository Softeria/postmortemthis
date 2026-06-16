//! `postmortem login`: get an OpenRouter API key via OAuth (PKCE) and save it
//! to the key file, so later runs need no OPENROUTER_API_KEY env var or --key.
//!
//! OpenRouter's PKCE flow: send the user to the auth page with an S256
//! challenge, receive a one-time code on a loopback callback, then exchange
//! the code plus the verifier for a key. No client secret, nothing to embed.

use crate::openrouter;
use anyhow::{Context, Result, bail};
use base64::Engine;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;

const AUTH_URL: &str = "https://openrouter.ai/auth";
const EXCHANGE_URL: &str = "https://openrouter.ai/api/v1/auth/keys";

pub fn run() -> Result<()> {
    let verifier = gen_verifier();
    let challenge = b64(&Sha256::digest(verifier.as_bytes()));

    // Loopback callback OpenRouter redirects to with the one-time code.
    let listener = TcpListener::bind("127.0.0.1:0").context("binding a loopback callback port")?;
    let port = listener.local_addr()?.port();
    let callback = format!("http://localhost:{port}");

    let auth = format!(
        "{AUTH_URL}?callback_url={}&code_challenge={}&code_challenge_method=S256",
        urlencode(&callback),
        challenge,
    );
    println!("Opening OpenRouter to authorize postmortem.");
    println!("If your browser does not open, paste this into it:\n\n  {auth}\n");
    let _ = open_browser(&auth);
    println!("Waiting for you to approve in the browser...");

    let code = wait_for_code(&listener).context("waiting for the OAuth callback")?;
    let key = exchange(&code, &verifier).context("exchanging the code for an API key")?;
    let path = save_key(&key).context("saving the key")?;

    println!("\nConnected. Saved your OpenRouter key to {}", path.display());
    println!("postmortem will use it automatically. Usage bills to your OpenRouter account.");
    Ok(())
}

/// A high-entropy PKCE verifier: 32 random bytes, base64url (43 chars).
fn gen_verifier() -> String {
    let mut buf = [0u8; 32];
    getrandom::getrandom(&mut buf).expect("OS randomness for the PKCE verifier");
    b64(&buf)
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Accept connections until one carries `?code=...`; reply with a small page
/// and return the code. Other hits (favicon, etc.) get a 204 and are ignored.
fn wait_for_code(listener: &TcpListener) -> Result<String> {
    for stream in listener.incoming() {
        let mut stream = stream?;
        let mut buf = [0u8; 4096];
        let n = stream.read(&mut buf).unwrap_or(0);
        let request = String::from_utf8_lossy(&buf[..n]);
        let line = request.lines().next().unwrap_or("");
        if let Some(code) = query_param(line, "code") {
            let body = "<html><body style=\"font-family:sans-serif\"><h2>postmortem is connected.</h2><p>You can close this tab and return to your terminal.</p></body></html>";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
            return Ok(code);
        }
        let _ = stream.write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n");
    }
    bail!("the callback listener closed before a code arrived")
}

/// Pull a query parameter out of an HTTP request line like
/// `GET /?code=abc&scope=x HTTP/1.1`.
fn query_param(request_line: &str, key: &str) -> Option<String> {
    let path = request_line.split_whitespace().nth(1)?;
    let query = path.split_once('?')?.1;
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=')
            && k == key
        {
            return Some(urldecode(v));
        }
    }
    None
}

/// Exchange the one-time code for an API key. OpenRouter verifies the code
/// against the S256 challenge we sent, so the verifier proves it is us.
fn exchange(code: &str, verifier: &str) -> Result<String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building a runtime for the token exchange")?;
    rt.block_on(async {
        let resp = reqwest::Client::new()
            .post(EXCHANGE_URL)
            .json(&serde_json::json!({
                "code": code,
                "code_verifier": verifier,
                "code_challenge_method": "S256",
            }))
            .send()
            .await
            .context("calling the OpenRouter token endpoint")?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("OpenRouter returned {status}: {}", text.trim());
        }
        let body: serde_json::Value =
            serde_json::from_str(&text).context("parsing the token response")?;
        body.get("key")
            .and_then(|k| k.as_str())
            .map(str::to_string)
            .context("the token response had no 'key' field")
    })
}

/// Write the key to the same file `openrouter::key()` reads, owner-only.
fn save_key(key: &str) -> Result<PathBuf> {
    let path = openrouter::key_file_path().context("cannot locate a home directory")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, key.trim())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(path)
}

/// Percent-encode a value for use in a query string.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Decode percent-escapes in a query value.
fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Best-effort open the URL in the user's browser. Failure is fine: the URL
/// is also printed for manual paste.
fn open_browser(url: &str) -> std::io::Result<()> {
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    } else if cfg!(target_os = "windows") {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    } else {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    cmd.stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
}
