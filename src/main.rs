// nanobot-rs: minimal Rust core of HKUDS/nanobot (agent loop + tools + memory).
#![allow(unexpected_cfgs)]
// Skipped (upstream has them): WebUI, chat channels, SQLite sessions, MCP,
// cron, subagent orchestration, multi-provider abstraction, SSE streaming
// (non-stream works fine for a CLI agent — switch to stream when UI latency matters).
use serde_json::{json, Value};
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

// ---- config (env only; ~/.nanobot.toml is a negligible nicety) ----
struct Cfg {
    api_base: String,
    api_key: String,
    model: String,
    workspace: PathBuf,
    exec_timeout: u64,
}

impl Cfg {
    fn from_env() -> Cfg {
        let workspace = std::env::var("NANOBOT_WORKSPACE").unwrap_or_else(|_| ".nanobot".into());
        let mut workspace = PathBuf::from(&workspace);
        if workspace.is_relative() {
            if let Ok(cwd) = std::env::current_dir() { workspace = cwd.join(workspace); }
        }
        Cfg {
            api_base: std::env::var("NANOBOT_API_BASE").unwrap_or_else(|_| "https://api.openai.com/v1".into()).trim_end_matches('/').to_string(),
            api_key: std::env::var("NANOBOT_API_KEY")
                .or_else(|_| std::env::var("OPENAI_API_KEY"))
                .expect("set NANOBOT_API_KEY (or OPENAI_API_KEY)"),
            model: std::env::var("NANOBOT_MODEL").unwrap_or_else(|_| "gpt-4o-mini".into()),
            workspace,
            exec_timeout: std::env::var("NANOBOT_EXEC_TIMEOUT").ok().and_then(|s| s.trim().parse().ok()).filter(|&s| s > 0).unwrap_or(60),
        }
    }
}

// ---- workspace-constrained file ops (trust boundary: never escape the root) ----
fn resolve(cfg: &Cfg, path: &str) -> Result<PathBuf, String> {
    let root = cfg.workspace.canonicalize().map_err(|e| format!("bad workspace: {e}"))?;
    let clean = path.trim();
    // fresh-review: only reject ".." as a whole component (a..b.txt is a legal name)
    if clean.starts_with('/') || clean.split(['/', '\\']).any(|c| c == "..") {
        return Err(format!("path must be workspace-relative and free of '..': {clean}"));
    }
    if clean.is_empty() { return Ok(root); }
    let joined = root.join(clean);
    // fresh-review: a symlink planted inside the workspace used to escape the root —
    // resolve every existing component and confirm the final target stays inside.
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    let mut probe = joined.clone();
    while !probe.exists() {
        match probe.file_name() {
            Some(n) => { missing.push(n.to_os_string()); probe.pop(); }
            None => break,
        }
    }
    let mut canon = probe.canonicalize().map_err(|e| format!("bad path: {e}"))?;
    for n in missing.into_iter().rev() { canon = canon.join(n); } // not-yet-created components are literal
    if !canon.starts_with(&root) {
        return Err(format!("path must stay inside the workspace: {clean}"));
    }
    Ok(canon)
}

fn tools_json() -> Value {
    let obj = |name: &str, desc: &str, params: Value| {
        json!({"type": "function", "function": {"name": name, "description": desc, "parameters": params}})
    };
    json!([
        obj("read_file", "Read a file from the workspace.", json!({"type":"object","properties":{"path":{"type":"string","description":"Relative path"}},"required":["path"]})),
        obj("write_file", "Write to a workspace file, overwriting it.", json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]})),
        obj("edit_file", "Exact string replace in a workspace file. old_string must match exactly once.", json!({"type":"object","properties":{"path":{"type":"string"},"old_string":{"type":"string"},"new_string":{"type":"string"}},"required":["path","old_string","new_string"]})),
        obj("list_dir", "List a workspace directory.", json!({"type":"object","properties":{"path":{"type":"string","description":"Dir, defaults to workspace root"}}})),
        obj("find_files", "List every file in the workspace (relative paths).", json!({"type":"object","properties":{}})),
        obj("grep", "Ripgrep pattern search across workspace files; falls back to a plain scan.", json!({"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string","description":"Optional subdir"}},"required":["pattern"]})),
        obj("exec", "Execute a shell command in the workspace.", json!({"type":"object","properties":{"command":{"type":"string"}},"required":["command"]})),
    ])
}

const READ_FILE_CAP: usize = 8 * 1024 * 1024;

// fresh-review: read_to_string used to load a 10GB file before any truncation ran;
// read at most cap+1 bytes so "oversized" is detectable without buffering the world.
fn read_file_capped(p: &Path, cap: usize) -> Result<(String, bool), String> {
    let mut f = std::fs::File::open(p).map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    (&mut f).take(cap as u64 + 1).read_to_end(&mut buf).map_err(|e| e.to_string())?;
    let oversized = buf.len() > cap;
    if oversized { buf.truncate(cap); }
    let mut s = String::from_utf8_lossy(&buf).into_owned();
    if oversized { s.push_str(&format!("\n...[file truncated at {cap} bytes]")); }
    Ok((s, oversized))
}

fn tool_exec(cfg: &Cfg, name: &str, a: &Value) -> Result<String, String> {
    let dir = cfg.workspace.clone();
    match name {
        "read_file" => {
            let p = resolve(cfg, a["path"].as_str().unwrap_or(""))?;
            Ok(read_file_capped(&p, READ_FILE_CAP)?.0)
        }
        "write_file" => {
            let p = resolve(cfg, a["path"].as_str().unwrap_or(""))?;
            if let Some(parent) = p.parent() { std::fs::create_dir_all(parent).map_err(|e| e.to_string())?; }
            std::fs::write(&p, a["content"].as_str().unwrap_or_default()).map_err(|e| e.to_string())?;
            Ok("ok".into())
        }
        "edit_file" => {
            let p = resolve(cfg, a["path"].as_str().unwrap_or(""))?;
            let (text, oversized) = read_file_capped(&p, READ_FILE_CAP)?;
            if oversized { return Err(format!("file too large for edit_file (>{} bytes); edit it via exec instead", READ_FILE_CAP)); }
            let old = a["old_string"].as_str().unwrap_or_default();
            let n = if old.is_empty() { 0 } else { text.matches(old).count() };
            if n != 1 {
                return Err(format!("old_string matches {n} times, need exactly 1"));
            }
            let text = text.replacen(old, a["new_string"].as_str().unwrap_or_default(), 1);
            std::fs::write(&p, text).map_err(|e| e.to_string())?;
            Ok("ok".into())
        }
        "list_dir" => {
            let p = resolve(cfg, a["path"].as_str().unwrap_or("."))?;
            let mut out: Vec<String> = std::fs::read_dir(&p).map_err(|e| e.to_string())?
                .flatten()
                .map(|e| {
                    let t = if e.file_type().map(|t| t.is_dir()).unwrap_or(false) { "d " } else { "  " };
                    format!("{}{}", t, e.file_name().to_string_lossy())
                })
                .collect();
            out.sort();
            Ok(if out.is_empty() { "(empty)".into() } else { out.join("\n") })
        }
        "find_files" => {
            let mut out: Vec<String> = Vec::new();
            let mut stack = vec![dir.clone()];
            'walk: while let Some(d) = stack.pop() {
                for e in std::fs::read_dir(&d).map_err(|e| e.to_string())?.flatten() {
                    let t = e.file_type().map_err(|e| e.to_string())?;
                    if t.is_dir() { stack.push(e.path()); }
                    out.push(e.path().strip_prefix(&dir).unwrap_or(&e.path()).to_string_lossy().into_owned());
                    // fresh-review: stop the walk once the result cap is reached
                    if out.len() >= 200 { break 'walk; }
                }
            }
            out.sort();
            Ok(if out.is_empty() { "No files found".into() } else { out.join("\n") })
        }
        "grep" => {
            let root = resolve(cfg, a["path"].as_str().unwrap_or("."))?;
            // fresh-review: a successful rg run with zero hits used to trigger a full
            // re-scan of the workspace; only an unavailable rg now falls back
            match Command::new("rg").current_dir(&dir)
                .args(["-n", "--", a["pattern"].as_str().unwrap_or("^$")]).arg(&root).output() {
                Ok(o) => {
                    let s = String::from_utf8_lossy(&o.stdout).lines().take(200).collect::<Vec<_>>().join("\n");
                    return Ok(if s.is_empty() { "No matches".into() } else { s });
                }
                Err(_) => { /* rg not installed: plain fallback below */ }
            }
            Ok(naive_grep(&dir, &root, a["pattern"].as_str().unwrap_or("")))
        }
        "exec" => {
            let cmd = a["command"].as_str().unwrap_or("");
            if cmd.trim().is_empty() { return Err("empty command".into()); }
            // fresh-review: a model-issued `sleep 999999` used to hang the loop forever
            let (stdout, stderr, status, truncated) = run_with_timeout(
                Command::new("sh").current_dir(&dir).args(["-c", cmd]),
                std::time::Duration::from_secs(cfg.exec_timeout))?;
            let status = match status {
                Some(c) => c,
                None => return Err(if truncated { "exec aborted: output exceeded the 8MB cap".into() }
                                   else { format!("exec timed out after {}s", cfg.exec_timeout) }),
            };
            let mut s = stdout;
            if truncated { s.push_str("\n...[output truncated at 8MB]"); }
            if !stderr.trim().is_empty() { s.push_str("\n--- stderr ---\n"); s.push_str(&stderr); }
            let lines: Vec<&str> = s.lines().take(400).collect();
            let s = lines.join("\n");
            // fresh-review: a signal-killed child used to report "exit code 0"
            Ok(if s.trim().is_empty() {
                match status.code() { Some(c) => format!("exit code {c}"), None => "terminated by signal".into() }
            } else { s })
        }
        _ => Err(format!("unknown tool {name}")),
    }
}

// fresh-review: run a child with a hard wall-clock deadline; returns (stdout, stderr,
// Option<exit-status>, truncated) — None means we killed it at the deadline. Reader
// threads drain the pipes concurrently so the child can't block on a full pipe, and
// stop after 8MB so a noisy child can't OOM the process either.
// fresh-review: run a child with a hard wall-clock deadline; returns (stdout, stderr,
// Option<exit-status>, truncated) — None exit-status means we killed it at the deadline.
// Reader threads drain the pipes concurrently so the child can't block on a full pipe,
// and each stream stops at 8MB so a noisy child can't OOM the process either.
fn run_with_timeout(cmd: &mut Command, timeout: std::time::Duration)
    -> Result<(String, String, Option<std::process::ExitStatus>, bool), String> {
    use std::process::Stdio;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let out_pipe = child.stdout.take().unwrap();
    let err_pipe = child.stderr.take().unwrap();
    let overflow = Arc::new(AtomicBool::new(false));
    const OUT_CAP: usize = 8 * 1024 * 1024;
    fn drain<R: std::io::Read>(mut pipe: R, of: &Arc<AtomicBool>) -> String {
        let mut s = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    s.extend_from_slice(&chunk[..n]);
                    if s.len() >= OUT_CAP { of.store(true, Ordering::Relaxed); break; }
                }
            }
        }
        String::from_utf8_lossy(&s).into_owned()
    }
    let of_out = overflow.clone();
    let of_err = overflow.clone();
    let t_out = std::thread::spawn(move || drain(out_pipe, &of_out));
    let t_err = std::thread::spawn(move || drain(err_pipe, &of_err));
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => {
                if std::time::Instant::now() >= deadline || overflow.load(Ordering::Relaxed) {
                    child.kill().ok();
                    let _ = child.wait(); // avoid zombie
                    break None;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => return Err(e.to_string()),
        }
    };
    let stdout = t_out.join().unwrap_or_default();
    let stderr = t_err.join().unwrap_or_default();
    Ok((stdout, stderr, status, overflow.load(Ordering::Relaxed)))
}

fn naive_grep(prefix: &Path, root: &Path, pat: &str) -> String {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                let t = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                let p = e.path();
                if t { stack.push(p); continue; }
                if let Ok((txt, _big)) = read_file_capped(&p, 2 * 1024 * 1024) {
                    for (i, line) in txt.lines().enumerate() {
                        if line.contains(pat) {
                            out.push(format!("{}:{}:{}", p.strip_prefix(prefix).unwrap_or(&p).to_string_lossy(), i + 1, line));
                            if out.len() >= 200 { return out.join("\n"); }
                        }
                    }
                }
            }
        }
    }
    if out.is_empty() { "No matches".into() } else { out.join("\n") }
}

// ---- prompt (mirrors upstream: identity + AGENTS/SOUL/USER + memory) ----
fn system_prompt(cfg: &Cfg) -> String {
    let ws = cfg.workspace.to_string_lossy().replace('\\', "/");
    let mut parts = vec![format!(
        "## Runtime\nnanobot-rs, Linux\n\n## Workspace\n- Agent profile: {ws}/SOUL.md and {ws}/USER.md\n- Long-term memory: {ws}/memory/MEMORY.md\n- History log: {ws}/memory/history.jsonl (append-only JSONL).\n\n## Tool Usage Notes\n- Treat a clear user request as authorization to complete the task, including execution and verification.\n- Ask for confirmation when an irreversible action requires it, or for clarification when essential information is missing.\n- Wait for tool results before writing the final answer."
    )];
    for f in ["AGENTS.md", "SOUL.md", "USER.md"] {
        if let Ok(c) = std::fs::read_to_string(cfg.workspace.join(f)) {
            let t = c.trim();
            if !t.is_empty() { parts.push(format!("## {f}\n\n{t}")); }
        }
    }
    if let Ok(c) = std::fs::read_to_string(cfg.workspace.join("memory/MEMORY.md")) {
        let t = c.trim();
        if !t.is_empty() { parts.push(t.to_string()); }
    }
    parts.join("\n\n---\n\n")
}

// ---- LLM call (OpenAI-compatible chat completions, non-stream) ----
fn chat(cfg: &Cfg, messages: &[Value]) -> Result<Value, String> {
    let body = serde_json::to_vec(&json!({
        "model": cfg.model,
        "messages": messages,
        "tools": tools_json(),
    })).map_err(|e| e.to_string())?;
    let resp = ureq::post(&format!("{}/chat/completions", cfg.api_base))
        .set("Authorization", &format!("Bearer {}", cfg.api_key))
        .set("Content-Type", "application/json")
        .timeout(std::time::Duration::from_secs(300))
        .send_bytes(&body).map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_reader(resp.into_reader()).map_err(|e| e.to_string())?;
    Ok(v)
}

const TOOL_ERROR_HINT: &str = "\n\n[Consider this result in the context of the user's goal and decide whether and how to proceed.]";

// ---- HTTP API (hand-rolled TinyHTTP, zero deps: serves /api/run, /api/tool, /health) ----
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

fn http_read_request(stream: &std::net::TcpStream) -> Result<(String, String, Vec<u8>), String> {
    // fresh-review: a stalled client must never pin a thread forever, and a single
    // header line must be rejected incrementally (read_line would buffer unbounded
    // bytes before our 8000 check could fire). Read line-by-line with a hard cap.
    stream.set_read_timeout(Some(std::time::Duration::from_secs(30))).map_err(|e| e.to_string())?;
    let mut reader = std::io::BufReader::new(stream);
    let mut line = String::new();
    if !read_line_capped(&mut reader, &mut line)? { return Err("bad request line".into()); }
    let mut parts = line.split_whitespace();
    let method = parts.next().ok_or("bad request line")?.to_string();
    parts.next().ok_or("bad request line")?.to_string();
    let mut clen = 0usize;
    let mut nheaders = 0u16;
    loop {
        let mut h = String::new();
        let got = read_line_capped(&mut reader, &mut h)?;
        if !got { break; } // clean EOF ends the header block
        let h = h.trim();
        if h.is_empty() { break; }
        nheaders += 1;
        if nheaders > 200 { return Err("too many headers".to_string()); }
        let low = h.to_ascii_lowercase();
        if let Some(v) = low.starts_with("content-length:").then(|| low["content-length:".len()..].trim()) {
            clen = v.split(',').next().unwrap_or("0").trim().parse().unwrap_or(0); // tolerate dup headers like "a, b"
        }
    }
    if clen > 4 * 1024 * 1024 { return Err("body too large".into()); }
    let mut body = vec![0u8; clen];
    reader.read_exact(&mut body).map_err(|e| e.to_string())?;
    Ok((method, line.trim().to_string(), body))
}

const LINE_CAP: usize = 8192;

// fresh-review: read one header/request line but stop reading at LINE_CAP bytes
// even without a newline, so an attacker line of any size costs us O(LINE_CAP) memory.
// Ok(true) = a line arrived (trailing newline optional), Ok(false) = clean EOF.
fn read_line_capped<R: std::io::BufRead>(reader: &mut R, buf: &mut String) -> Result<bool, String> {
    buf.clear();
    let mut n = 0usize;
    loop {
        let avail = reader.fill_buf().map_err(|e| e.to_string())?;
        if avail.is_empty() { return Ok(!buf.is_empty()); } // EOF: emit what we collected
        match avail.iter().position(|&b| b == b'\n') {
            Some(pos) => {
                n += pos + 1;
                if n > LINE_CAP { return Err("line too long".into()); }
                buf.push_str(&String::from_utf8_lossy(&avail[..pos])); // exclude the newline
                reader.consume(pos + 1);
                return Ok(true);
            }
            None => {
                n += avail.len();
                if n > LINE_CAP { return Err("line too long".into()); }
                buf.push_str(&String::from_utf8_lossy(avail));
                let len = avail.len();
                reader.consume(len);
            }
        }
    }
}

fn http_respond(stream: &mut std::net::TcpStream, code: u16, msg: &str, body: &[u8]) {
    let head = format!("HTTP/1.1 {code} {msg}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn session_store() -> Arc<Mutex<HashMap<String, Vec<Value>>>> {
    // ponytail: sessions in memory; SQLite persistence moved to the Go gateway (best-fit there)
    Arc::new(Mutex::new(HashMap::new()))
}

fn serve(cfg: Cfg) -> ! {
    let port: u16 = std::env::var("NANOBOT_SERVE").unwrap_or_else(|_| "8080".into()).trim().parse().unwrap_or(8080);
    let sessions = session_store();
    let cfg = Arc::new(cfg);
    let addr = format!("127.0.0.1:{port}");
    let listener = std::net::TcpListener::bind(&addr).expect("bind");
    eprintln!("nanobot-rs serve on http://{addr}");
    for stream in listener.incoming() {
        let mut stream = match stream { Ok(s) => s, Err(_) => continue };
        let cfg = Arc::clone(&cfg);
        let sessions = Arc::clone(&sessions);
        std::thread::spawn(move || {
            let (method, reqline, body) = match http_read_request(&stream) { Ok(r) => r, Err(e) => { http_respond(&mut stream, 400, "Bad Request", e.as_bytes()); return; } };
            let path = reqline.split(' ').nth(1).unwrap_or("").split('?').next().unwrap_or("").to_string();
            // fresh-review: prefix matching let /healthzzz and /api/toolzzz through; match exactly
            if path == "/health" {
                http_respond(&mut stream, 200, "OK", br#"{"ok":true}"#);
                return;
            }
            if path == "/api/tool" && method == "POST" {
                let v: Value = match serde_json::from_slice(&body) {
                    Ok(v) => v,
                    Err(_) => { http_respond(&mut stream, 400, "Bad Request", br#"{"error":"bad json body"}"#); return; }
                };
                let name = v["name"].as_str().unwrap_or("").to_string();
                let args = v["arguments"].clone();
                let out = match tool_exec(&cfg, &name, &args) {
                    Ok(s) => json!({"ok": true, "result": truncate(&s, 20_000)}),
                    Err(e) => json!({"ok": false, "error": truncate(&e, 8_000)}),
                };
                http_respond(&mut stream, 200, "OK", serde_json::to_vec(&out).unwrap().as_slice());
                return;
            }
            if path == "/api/run" && method == "POST" {
                let v: Value = match serde_json::from_slice(&body) {
                    Ok(v) => v,
                    Err(_) => { http_respond(&mut stream, 400, "Bad Request", br#"{"error":"bad json body"}"#); return; }
                };
                let prompt = v["prompt"].as_str().unwrap_or("").to_string();
                if prompt.trim().is_empty() { http_respond(&mut stream, 400, "Bad Request", br#"{"error":"empty prompt"}"#); return; }
                let sid = v["session_id"].as_str().unwrap_or("").to_string();
                let mut msgs: Vec<Value> = vec![json!({"role": "system", "content": system_prompt(&cfg)})];
                if !sid.is_empty() {
                    msgs.extend(sessions.lock().unwrap().get(&sid).cloned().unwrap_or_default());
                }
                msgs.push(json!({"role": "user", "content": prompt}));
                let mut final_text = String::new();
                for _ in 0..32 {
                    let r = match chat(&cfg, &msgs) { Ok(v) => v, Err(e) => { final_text = format!("chat error: {e}"); break; } };
                    let msg = r["choices"][0]["message"].clone();
                    if !msg.is_object() { final_text = r.to_string(); break; }
                    msgs.push(msg.clone());
                    if let Some(tcs) = msg["tool_calls"].as_array().cloned() {
                        for tc in tcs {
                            let name = tc["function"]["name"].as_str().unwrap_or("?").to_string();
                            let args: Value = serde_json::from_str(tc["function"]["arguments"].as_str().unwrap_or("{}")).unwrap_or(json!({}));
                            let result = match tool_exec(&cfg, &name, &args) {
                                Ok(s) => truncate(&s, 20_000),
                                Err(e) => format!("{}{TOOL_ERROR_HINT}", truncate(&e, 8_000)),
                            };
                            msgs.push(json!({"role": "tool", "tool_call_id": tc["id"].clone(), "content": result}));
                        }
                    } else {
                        final_text = msg["content"].as_str().unwrap_or("").to_string();
                        break;
                    }
                }
                if !sid.is_empty() {
                    let mut store = sessions.lock().unwrap();
                    // fresh-review: session ids arrive from clients; cap the count so memory can't grow forever
                    if store.len() > 200 && !store.contains_key(&sid) {
                        if let Some(k) = store.keys().next().cloned() { store.remove(&k); }
                    }
                    let e = store.entry(sid).or_default();
                    e.push(json!({"role": "user", "content": prompt}));
                    // fresh-review: persisting a dangling role:"tool" reply (chat error or
                    // 32-round exit) poisons the next replay with an API 400 — store only
                    // plain assistant content replies
                    let ok_last = msgs.last()
                        .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("assistant"))
                        .filter(|m| m.get("tool_calls").is_none())
                        .filter(|m| m.get("content").and_then(|c| c.as_str()).is_some());
                    if let Some(m) = ok_last { e.push(m.clone()); }
                    // eviction keeps whole user/assistant turns; never split a pair
                    while e.len() > 200 {
                        let r0 = e[0].get("role").and_then(|r| r.as_str()).unwrap_or("");
                        let drop = if r0 == "user" && e.len() > 1
                            && e[1].get("role").and_then(|r| r.as_str()) == Some("assistant") { 2 } else { 1 };
                        if e.len() <= drop { break; }
                        e.drain(0..drop);
                    }
                }
                // fresh-review: falling out of the 32 tool rounds left an empty reply with no hint
                if final_text.is_empty() { final_text = "(stopped after 32 tool rounds)".into(); }
                let out = json!({"text": final_text});
                http_respond(&mut stream, 200, "OK", serde_json::to_vec(&out).unwrap().as_slice());
                return;
            }
            http_respond(&mut stream, 404, "Not Found", br#"{"error":"unknown"}"#);
        });
    }
    unreachable!()
}

// ---- agent loop ----
fn main() {
    let cfg = Cfg::from_env();
    let _ = std::fs::create_dir_all(cfg.workspace.join("memory"));
    let hist_file = cfg.workspace.join("memory/history.jsonl");
    eprintln!("nanobot-rs ready. model={} workspace={}", cfg.model, cfg.workspace.display());
    if std::env::var("NANOBOT_SERVE").is_ok() { serve(cfg); }
    let mut messages = vec![json!({"role": "system", "content": system_prompt(&cfg)})];
    loop {
        print!("> "); let _ = std::io::stdout().flush();
        let mut line = String::new();
        if std::io::stdin().lock().read_line(&mut line).unwrap_or(0) == 0 { break; }
        let line = line.trim().to_string();
        if line.is_empty() { continue; }
        if line == "/exit" || line == "/quit" { break; }
        messages.push(json!({"role": "user", "content": line}));

        loop {
            let r = match chat(&cfg, &messages) {
                Ok(v) => v,
                Err(e) => { eprintln!("chat error: {e}" ); break; }
            };
            let msg = r["choices"][0]["message"].clone();
            if !msg.is_object() { eprintln!("bad response: {r}"); break; }
            messages.push(msg.clone());
            if let Some(tcs) = msg["tool_calls"].as_array().cloned() {
                for tc in tcs {
                    let name = tc["function"]["name"].as_str().unwrap_or("?").to_string();
                    let args: Value = serde_json::from_str(tc["function"]["arguments"].as_str().unwrap_or("{}")).unwrap_or(json!({}));
                    let result = match tool_exec(&cfg, &name, &args) {
                        Ok(s) => truncate(&s, 20_000),
                        Err(e) => format!("{}{TOOL_ERROR_HINT}", truncate(&e, 8_000)),
                    };
                    eprintln!("  [{name}] {} bytes", result.len());
                    messages.push(json!({"role": "tool", "tool_call_id": tc["id"].clone(), "content": result}));
                }
            } else {
                println!("{}", msg["content"].as_str().unwrap_or("").trim_end());
                break;
            }
        }
        // ponytail: history is an append-only audit log, not replayed into context (upstream SQLite sessions do that — add when transcript growth hurts)
        // fresh-review: the CLI transcript used to grow forever and never recover once
        // past context; trim whole user/assistant turns, never splitting a pending
        // assistant(tool_calls)+tool pair off from each other
        while messages.len() > 201
            && messages[1].get("role").and_then(|r| r.as_str()) == Some("user")
            && messages.get(2).and_then(|m| m.get("role")).and_then(|r| r.as_str()) == Some("assistant")
            && messages[2].get("tool_calls").is_none()
        {
            messages.drain(1..3);
        }
        let _ = std::fs::OpenOptions::new().create(true).append(true)
            .open(&hist_file).and_then(|mut f| writeln!(f, "{}", json!({"at": log_secs(), "user": line})));
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max { return s.into(); }
    let mut cut = max;
    while cut > 0 && !s.is_char_boundary(cut) { cut -= 1; }
    format!("{}\n...[truncated {} bytes]", &s[..cut], s.len() - cut)
}
fn log_secs() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) }

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp_ws(name: &str) -> (Cfg, PathBuf) {
        let dir = std::env::temp_dir().join(format!("nbot-rs-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = Cfg { api_base: String::new(), api_key: String::new(), model: String::new(), workspace: dir.clone(), exec_timeout: 5 };
        (cfg, dir)
    }

    #[test]
    fn resolve_blocks_symlink_escape() {
        let (cfg, _d) = tmp_ws("link");
        let dir = cfg.workspace.clone();
        let parent = dir.parent().unwrap();
        std::os::unix::fs::symlink(parent, dir.join("link")).unwrap();
        // write through the symlink must be refused, file must not appear outside
        let bad = tool_exec(&cfg, "write_file", &json!({"path":"link/escape.txt","content":"x"}));
        assert!(bad.is_err(), "{bad:?}");
        assert!(!parent.join("escape.txt").exists());
        // nested traversal through the link is also refused
        let bad2 = tool_exec(&cfg, "read_file", &json!({"path":"link/sub/../../escaped.txt"}));
        assert!(bad2.is_err());
        // an ordinary write still works
        let ok = tool_exec(&cfg, "write_file", &json!({"path":"sub/new.txt","content":"in"}));
        assert_eq!(ok.unwrap(), "ok");
    }

    #[test]
    fn read_file_truncates_huge_file() {
        let (cfg, d) = tmp_ws("huge");
        std::fs::write(d.join("big.txt"), "x".repeat(READ_FILE_CAP + 100)).unwrap();
        let out = tool_exec(&cfg, "read_file", &json!({"path":"big.txt"})).unwrap();
        assert!(out.contains("file truncated"), "{out}");
        assert!(out.len() < READ_FILE_CAP + 200);
        // large files degrade read_file but refuse edit_file, forcing exec-level tools
        let edit = tool_exec(&cfg, "edit_file", &json!({"path":"big.txt","old_string":"x","new_string":"y"}));
        assert!(edit.is_err());
    }

    #[test]
    fn resolve_blocks_escape() {
        let (cfg, _d) = tmp_ws("esc");
        assert!(resolve(&cfg, "../etc/passwd").is_err());
        assert!(resolve(&cfg, "/etc/passwd").is_err());
        assert_eq!(resolve(&cfg, "a/b.txt").unwrap(), cfg.workspace.join("a/b.txt"));
    }

    #[test]
    fn edit_file_requires_unique_match() {
        let (cfg, _d) = tmp_ws("edit");
        std::fs::write(cfg.workspace.join("f.txt"), "hello world\n").unwrap();
        let ok = tool_exec(&cfg, "edit_file", &json!({"path":"f.txt","old_string":"world","new_string":"rust"}));
        assert_eq!(ok.unwrap(), "ok");
        let bad = tool_exec(&cfg, "edit_file", &json!({"path":"f.txt","old_string":"","new_string":"x"}));
        assert!(bad.is_err());
        assert!(std::fs::read_to_string(cfg.workspace.join("f.txt")).unwrap().contains("hello rust"));
    }

    #[test]
    fn exec_runs_in_workspace_and_lists() {
        let (cfg, d) = tmp_ws("exec");
        std::fs::write(d.join("x.txt"), "hi").unwrap();
        let out = tool_exec(&cfg, "exec", &json!({"command":"ls"})).unwrap();
        assert!(out.contains("x.txt"), "{out}");
        let list = tool_exec(&cfg, "list_dir", &json!({})).unwrap();
        assert!(list.contains("x.txt"));
    }

    #[test]
    fn truncate_respects_char_boundary() {
        let s = "中中中中中";
        assert!(!truncate(s, 3).starts_with("�"));
        assert_eq!(truncate("short", 100), "short");
    }

    #[test]
    fn grep_finds_pattern() {
        let (cfg, d) = tmp_ws("grep");
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("sub/g.txt"), "needle here\nnothing\n").unwrap();
        let out = tool_exec(&cfg, "grep", &json!({"pattern":"needle"})).unwrap();
        assert!(out.contains("needle here"), "{out}");
    }

    #[test]
    fn http_reads_lowercase_content_length() {
        // Node fetch sends lowercase header names; regression for the MCP bridge
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let cli = std::net::TcpStream::connect(addr).unwrap();
        let srv = listener.accept().unwrap().0;
        let mut cli2 = cli.try_clone().unwrap();
        std::thread::spawn(move || {
            use std::io::Write;
            let _ = cli2.write_all(b"POST /api/tool HTTP/1.1\r\ncontent-length: 2\r\n\r\n{}");
        });
        let (method, _line, body) = http_read_request(&srv).unwrap();
        assert_eq!(method, "POST");
        assert_eq!(body, b"{}");
    }

    #[test]
    fn http_rejects_header_flood() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let cli = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let srv = listener.accept().unwrap().0;
        let mut cli2 = cli.try_clone().unwrap();
        drop(cli); // close our copy: the server must see EOF after the flood
        std::thread::spawn(move || {
            use std::io::Write;
            let mut junk = b"GET / HTTP/1.1\r\n".to_vec();
            for i in 0..400 { junk.extend_from_slice(format!("x{i}: v\r\n").as_bytes()); }
            let _ = cli2.write_all(&junk);
        });
        assert!(http_read_request(&srv).is_err());
    }

    #[test]
    fn http_rejects_oversized_line() {
        // fresh-review regression: one header line of N bytes costs the server O(8KB),
        // not O(N) — read_line used to buffer the whole line before any cap could fire.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let cli = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let srv = listener.accept().unwrap().0;
        let mut cli2 = cli.try_clone().unwrap();
        drop(cli); // the server must see EOF after our single giant line
        std::thread::spawn(move || {
            use std::io::Write;
            let mut big = b"GET / HTTP/1.1\r\nx-big: ".to_vec();
            big.extend_from_slice(&vec![b'A'; 3_000_000]);
            big.extend_from_slice(b"\r\n\r\n");
            let _ = cli2.write_all(&big);
        });
        assert!(http_read_request(&srv).is_err());
    }

    #[test]
    fn exec_timeout_kills_sleep() {
        let (cfg, _d) = tmp_ws("timeout");
        let t0 = std::time::Instant::now();
        let err = tool_exec(&cfg, "exec", &json!({"command":"sleep 30"})).unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        assert!(t0.elapsed() < std::time::Duration::from_secs(15));
        // a normal command after a timed-out one must still work
        assert!(tool_exec(&cfg, "exec", &json!({"command":"echo ok"})).unwrap().contains("ok"));
    }
}
