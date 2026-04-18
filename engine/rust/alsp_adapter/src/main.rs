use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

static WARMED_UP: AtomicBool = AtomicBool::new(false);
static WARMUP_SNAPSHOTS: LazyLock<Mutex<HashMap<String, HashMap<String, u64>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static ACTIVE_PROJECTS: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Debug, Deserialize)]
struct RequestEnvelope {
    version: String,
    request_id: String,
    trace_id: String,
    #[allow(dead_code)]
    session_id: Option<String>,
    #[allow(dead_code)]
    source: String,
    #[allow(dead_code)]
    target: String,
    action: String,
    #[allow(dead_code)]
    payload: Value,
    #[allow(dead_code)]
    meta: Option<Value>,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    code: String,
    message: String,
    retryable: bool,
    node: String,
    details: Value,
}

#[derive(Debug, Serialize)]
struct ResponseEnvelope {
    version: String,
    request_id: String,
    trace_id: String,
    status: String,
    data: Option<Value>,
    error: Option<ErrorBody>,
    fallback_used: bool,
    cost: Value,
}

#[tokio::main]
async fn main() -> Result<()> {
    // L0-P0-01 baseline: TCP localhost transport.
    let addr = std::env::var("ALSP_ADAPTER_ADDR").unwrap_or_else(|_| "127.0.0.1:8787".to_string());
    let listener = TcpListener::bind(&addr).await?;
    println!("alsp_adapter listening on {addr}");

    loop {
        let (socket, peer) = listener.accept().await?;
        tokio::spawn(async move {
            if let Err(err) = handle_connection(socket).await {
                eprintln!("connection error from {peer}: {err}");
            }
        });
    }
}

async fn handle_connection(socket: TcpStream) -> Result<()> {
    let (reader, mut writer) = socket.into_split();
    let mut lines = BufReader::new(reader).lines();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }

        if line.starts_with("GET ") {
            while let Some(header_line) = lines.next_line().await? {
                if header_line.trim().is_empty() {
                    break;
                }
            }
            let path = line.split_whitespace().nth(1).unwrap_or("/");
            let (status_code, body) = http_response_body(path);
            let reason = if status_code == 200 {
                "OK"
            } else {
                "Not Found"
            };
            let resp = format!(
                "HTTP/1.1 {} {}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                status_code,
                reason,
                body.as_bytes().len(),
                body
            );
            writer.write_all(resp.as_bytes()).await?;
            writer.flush().await?;
            return Ok(());
        }

        let response = match serde_json::from_str::<RequestEnvelope>(&line) {
            Ok(req) => handle_request(req),
            Err(err) => ResponseEnvelope {
                version: "v0".to_string(),
                request_id: "unknown".to_string(),
                trace_id: "unknown".to_string(),
                status: "error".to_string(),
                data: None,
                error: Some(ErrorBody {
                    code: "COMMON_BAD_REQUEST".to_string(),
                    message: format!("invalid json request: {err}"),
                    retryable: false,
                    node: "Adapter".to_string(),
                    details: json!({}),
                }),
                fallback_used: false,
                cost: json!({ "duration_ms": 0 }),
            },
        };

        let payload = serde_json::to_string(&response)?;
        writer.write_all(payload.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
    }

    Ok(())
}

fn http_response_body(path: &str) -> (u16, String) {
    let normalized_path = path.split('?').next().unwrap_or(path);
    let body = match normalized_path {
        "/health" => json!({
            "status": "ok",
            "service": "lesscoder-alsp-adapter",
            "protocol_version": "v0",
            "warmed_up": WARMED_UP.load(Ordering::SeqCst),
        }),
        "/methods" => {
            let health = system_health_payload(any_active_project());
            json!({
                "status": "ok",
                "methods": health.get("methods").cloned().unwrap_or(json!([])),
                "summary": health.get("summary").cloned().unwrap_or(json!({})),
                "warmed_up": health.get("warmed_up").cloned().unwrap_or(json!(false)),
                "active_project": health.get("active_project").cloned().unwrap_or(Value::Null),
            })
        }
        _ => {
            return (
                404,
                json!({
                    "status": "error",
                    "error_code": "HTTP_NOT_FOUND",
                    "message": "endpoint not found",
                    "available": ["/health", "/methods"]
                })
                .to_string(),
            )
        }
    };
    (200, body.to_string())
}

fn handle_request(req: RequestEnvelope) -> ResponseEnvelope {
    if req.version != "v0" {
        return ResponseEnvelope {
            version: "v0".to_string(),
            request_id: req.request_id,
            trace_id: req.trace_id,
            status: "error".to_string(),
            data: None,
            error: Some(ErrorBody {
                code: "COMMON_BAD_REQUEST".to_string(),
                message: format!("unsupported version: {}", req.version),
                retryable: false,
                node: "Adapter".to_string(),
                details: json!({ "supported_version": "v0" }),
            }),
            fallback_used: false,
            cost: json!({ "duration_ms": 0 }),
        };
    }

    route_action(req)
}

fn route_action(req: RequestEnvelope) -> ResponseEnvelope {
    match req.action.as_str() {
        "system.health" => {
            let active = active_project_for_request(&req);
            ok_response(req, system_health_payload(active))
        }
        "project.activate" => {
            let project_root = match extract_root_path(&req.payload) {
                Some(v) => v,
                None => {
                    return error_response(
                        req,
                        "COMMON_BAD_REQUEST",
                        "project.activate requires explicit project_root/path",
                        false,
                        "Adapter",
                        json!({
                            "required": ["project_root|path"],
                            "example": {
                                "action": "project.activate",
                                "payload": {"project_root": "<project-root>"}
                            }
                        }),
                    )
                }
            };
            register_active_project(&req, &project_root);
            let context_key = context_key_for_request(&req);
            ok_response(
                req,
                json!({
                    "status": "ok",
                    "active_project": project_root,
                    "context_key": context_key,
                }),
            )
        }
        "system.warmup" => {
            let started = Instant::now();
            let project_root = match extract_root_path(&req.payload) {
                Some(v) => v,
                None => {
                    return error_response(
                        req,
                        "COMMON_BAD_REQUEST",
                        "system.warmup requires explicit project_root/path",
                        false,
                        "Adapter",
                        json!({
                            "required": ["project_root|path"],
                            "example": {
                                "action": "system.warmup",
                                "payload": {"project_root": "<project-root>"}
                            }
                        }),
                    )
                }
            };
            register_active_project(&req, &project_root);
            let warmup_result = compute_incremental_warmup(PathBuf::from(&project_root));
            WARMED_UP.store(true, Ordering::SeqCst);
            match warmup_result {
                Ok((changed_files, reindexed_files)) => ok_response(
                    req,
                    json!({
                        "status": "ready",
                        "adapter": "alsp_adapter",
                        "message": "internal warmup completed",
                        "project_root": project_root,
                        "changed_files": changed_files,
                        "reindexed_files": reindexed_files,
                        "duration_ms": started.elapsed().as_millis(),
                    }),
                ),
                Err(err) => internal_error_response(
                    req,
                    "COMMON_INTERNAL",
                    format!("system.warmup failed: {err}"),
                ),
            }
        }
        "repo.map" => {
            let root = extract_root_path(&req.payload).unwrap_or_else(|| ".".to_string());
            let root_path = PathBuf::from(&root);
            match alsp::build_java_repo_map(&root_path) {
                Ok(map) => ok_response(req, serde_json::to_value(map).unwrap_or(json!({}))),
                Err(err) => internal_error_response(
                    req,
                    "COMMON_INTERNAL",
                    format!("repo.map failed: {err}"),
                ),
            }
        }
        "symbol.lookup" | "symbol.lookup.static" | "symbol.resolve" => {
            if !WARMED_UP.load(Ordering::SeqCst) {
                let action = req.action.clone();
                return precondition_required_response(req, action.as_str());
            }
            let action_name = req.action.clone();
            let root = match resolve_project_root_for_action(&req) {
                Some(v) => v,
                None => return project_activation_required_response(req, action_name.as_str()),
            };
            let symbol = req
                .payload
                .get("symbol")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let Some(symbol) = symbol else {
                return bad_request_response(req, "missing payload.symbol");
            };
            if req.action == "symbol.lookup"
                && req
                    .payload
                    .get("force_lsp_timeout")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
            {
                return error_response(
                    req,
                    "ALSP_LSP_TIMEOUT",
                    "simulated lsp timeout",
                    true,
                    "Analyze",
                    json!({"symbol": symbol}),
                );
            }
            match alsp::lookup_java_symbol(&PathBuf::from(&root), &symbol) {
                Ok(Some(loc)) => ok_response(req, serde_json::to_value(loc).unwrap_or(json!({}))),
                Ok(None) => error_response(
                    req,
                    "ALSP_SYMBOL_NOT_FOUND",
                    "symbol not found",
                    false,
                    "Analyze",
                    json!({"symbol": symbol}),
                ),
                Err(err) => internal_error_response(
                    req,
                    "COMMON_INTERNAL",
                    format!("symbol.lookup failed: {err}"),
                ),
            }
        }
        "symbol.lookup.fuzzy" => {
            if !WARMED_UP.load(Ordering::SeqCst) {
                return precondition_required_response(req, "symbol.lookup.fuzzy");
            }
            let root = match resolve_project_root_for_action(&req) {
                Some(v) => v,
                None => return project_activation_required_response(req, "symbol.lookup.fuzzy"),
            };
            let symbol = req
                .payload
                .get("symbol")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let Some(symbol) = symbol else {
                return bad_request_response(req, "missing payload.symbol");
            };
            let limit = parse_limit(&req.payload, 10, 50);
            match alsp::lookup_java_symbol_fuzzy(&PathBuf::from(&root), &symbol, limit) {
                Ok(items) if !items.is_empty() => ok_response(
                    req,
                    json!({
                        "symbol": symbol,
                        "limit": limit,
                        "items": items,
                    }),
                ),
                Ok(_) => error_response(
                    req,
                    "ALSP_SYMBOL_NOT_FOUND",
                    "symbol not found",
                    false,
                    "Analyze",
                    json!({"symbol": symbol}),
                ),
                Err(err) => internal_error_response(
                    req,
                    "COMMON_INTERNAL",
                    format!("symbol.lookup.fuzzy failed: {err}"),
                ),
            }
        }
        "graph.calls" => {
            if !WARMED_UP.load(Ordering::SeqCst) {
                return precondition_required_response(req, "graph.calls");
            }
            let root = match resolve_project_root_for_action(&req) {
                Some(v) => v,
                None => return project_activation_required_response(req, "graph.calls"),
            };
            let symbol = req
                .payload
                .get("symbol")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let Some(symbol) = symbol else {
                return bad_request_response(req, "missing payload.symbol");
            };
            let depth = req
                .payload
                .get("depth")
                .and_then(|v| v.as_u64())
                .and_then(|v| usize::try_from(v).ok())
                .unwrap_or(1);
            let language = detect_graph_language(&req.payload, &root);
            if language == "typescript" {
                match alsp::graph_calls_typescript_lsp(Path::new(&root), &symbol, depth) {
                    Ok(result) => {
                        ok_response(req, serde_json::to_value(result).unwrap_or(json!({})))
                    }
                    Err(err) => {
                        let msg = err.to_string();
                        if msg.contains("LSP_UNAVAILABLE") {
                            error_response(
                                req,
                                "ALSP_LSP_UNAVAILABLE",
                                "typescript language server unavailable",
                                true,
                                "Analyze",
                                json!({"language": "typescript", "symbol": symbol, "project_root": root}),
                            )
                        } else if msg.contains("LSP_TIMEOUT") {
                            error_response(
                                req,
                                "ALSP_LSP_TIMEOUT",
                                "typescript language server timeout",
                                true,
                                "Analyze",
                                json!({"language": "typescript", "symbol": symbol, "project_root": root}),
                            )
                        } else {
                            internal_error_response(
                                req,
                                "COMMON_INTERNAL",
                                format!("graph.calls failed: {err}"),
                            )
                        }
                    }
                }
            } else {
                ok_response(
                    req,
                    json!({
                        "symbol": symbol,
                        "depth": depth,
                        "calls": [],
                        "provider": "alsp.graph.calls.v0",
                        "language": language,
                        "truncated": false
                    }),
                )
            }
        }
        "patch.apply" => {
            let target = req
                .payload
                .get("target")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let Some(target) = target else {
                return bad_request_response(req, "missing payload.target");
            };
            let block = req
                .payload
                .get("search_replace_blocks")
                .and_then(|v| v.as_array())
                .and_then(|arr| arr.first())
                .cloned();
            let Some(block) = block else {
                return bad_request_response(req, "missing payload.search_replace_blocks[0]");
            };
            let search = block.get("search").and_then(|v| v.as_str()).unwrap_or("");
            let replace = block.get("replace").and_then(|v| v.as_str()).unwrap_or("");
            let force_conflict = req
                .payload
                .get("force_patch_conflict")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if search.is_empty() {
                return bad_request_response(req, "empty search pattern");
            }
            if force_conflict {
                return error_response(
                    req,
                    "PATCHLET_CONFLICT",
                    "simulated patch conflict",
                    false,
                    "Execute",
                    json!({"file": target, "search_excerpt": search}),
                );
            }

            match patchlet::apply_search_replace_with_backup(
                PathBuf::from(&target).as_path(),
                search,
                replace,
            ) {
                Ok(result) => {
                    if result.replacements == 0 {
                        error_response(
                            req,
                            "PATCHLET_SEARCH_MISS",
                            "search block not found in target file",
                            false,
                            "Execute",
                            json!({"file": target, "search_excerpt": search}),
                        )
                    } else {
                        ok_response(req, serde_json::to_value(result).unwrap_or(json!({})))
                    }
                }
                Err(err) => error_response(
                    req,
                    "PATCHLET_APPLY_FAILED",
                    &format!("patch apply failed: {err}"),
                    true,
                    "Execute",
                    json!({"file": target, "search_excerpt": search}),
                ),
            }
        }
        _ => error_response(
            req,
            "ADAPTER_ROUTE_FAILED",
            "unsupported action",
            true,
            "Adapter",
            json!({}),
        ),
    }
}

fn system_health_payload(active_project: Option<String>) -> Value {
    let methods = vec![
        json!({
            "action": "system.health",
            "category": "system",
            "requires_warmup": false,
            "requires_project_activation": false,
            "available": true,
            "description": "service health and capability listing",
        }),
        json!({
            "action": "project.activate",
            "category": "system",
            "requires_warmup": false,
            "requires_project_activation": false,
            "available": true,
            "description": "bind active project root to current session/trace context",
        }),
        json!({
            "action": "system.warmup",
            "category": "system",
            "requires_warmup": false,
            "requires_project_activation": false,
            "available": true,
            "description": "preheat runtime and semantic index",
        }),
        json!({
            "action": "repo.map",
            "category": "context",
            "requires_warmup": false,
            "requires_project_activation": false,
            "available": true,
            "description": "repository skeleton map",
        }),
        json!({
            "action": "symbol.lookup",
            "category": "context",
            "requires_warmup": true,
            "requires_project_activation": true,
            "available": true,
            "description": "symbol resolution (LSP preferred)",
        }),
        json!({
            "action": "symbol.lookup.static",
            "category": "context",
            "requires_warmup": true,
            "requires_project_activation": true,
            "available": true,
            "description": "symbol resolution from static index",
        }),
        json!({
            "action": "symbol.resolve",
            "category": "context",
            "requires_warmup": true,
            "requires_project_activation": true,
            "available": true,
            "description": "canonical symbol resolve alias",
        }),
        json!({
            "action": "symbol.lookup.fuzzy",
            "category": "context",
            "requires_warmup": true,
            "requires_project_activation": true,
            "available": true,
            "description": "fuzzy symbol lookup (prefix/contains ranking)",
        }),
        json!({
            "action": "patch.apply",
            "category": "edit",
            "requires_warmup": false,
            "requires_project_activation": false,
            "available": true,
            "description": "apply search/replace patch",
        }),
        json!({
            "action": "graph.calls",
            "category": "graph",
            "requires_warmup": true,
            "requires_project_activation": true,
            "available": true,
            "description": "call graph query",
        }),
    ];

    let requires_warmup_count = methods
        .iter()
        .filter(|m| {
            m.get("requires_warmup")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        })
        .count();
    let requires_activation_count = methods
        .iter()
        .filter(|m| {
            m.get("requires_project_activation")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        })
        .count();

    json!({
        "status": "ok",
        "service": "lesscoder-alsp-adapter",
        "protocol_version": "v0",
        "warmed_up": WARMED_UP.load(Ordering::SeqCst),
        "active_project": active_project,
        "methods": methods,
        "summary": {
            "total_methods": 10,
            "requires_warmup_methods": requires_warmup_count,
            "requires_project_activation_methods": requires_activation_count,
        }
    })
}

fn parse_limit(payload: &Value, default_limit: usize, max_limit: usize) -> usize {
    let maybe_limit = payload
        .get("limit")
        .and_then(|v| v.as_u64())
        .and_then(|v| usize::try_from(v).ok());
    match maybe_limit {
        Some(v) if v >= 1 => v.min(max_limit),
        _ => default_limit,
    }
}

fn extract_root_path(payload: &Value) -> Option<String> {
    payload
        .get("path")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            payload
                .get("project_root")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
}

fn detect_graph_language(payload: &Value, project_root: &str) -> String {
    if let Some(explicit) = payload.get("language").and_then(|v| v.as_str()) {
        let normalized = explicit.trim().to_ascii_lowercase();
        if !normalized.is_empty() {
            return normalized;
        }
    }
    if workspace_has_typescript_files(Path::new(project_root)) {
        "typescript".to_string()
    } else {
        "java".to_string()
    }
}

fn workspace_has_typescript_files(root: &Path) -> bool {
    if !root.exists() {
        return false;
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(v) => v,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if matches!(
                path.extension().and_then(|x| x.to_str()),
                Some("ts") | Some("tsx") | Some("js") | Some("jsx")
            ) {
                return true;
            }
        }
    }
    false
}

fn context_key_for_request(req: &RequestEnvelope) -> String {
    req.session_id
        .as_ref()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| req.trace_id.clone())
}

fn active_project_for_request(req: &RequestEnvelope) -> Option<String> {
    let key = context_key_for_request(req);
    ACTIVE_PROJECTS
        .lock()
        .ok()
        .and_then(|m| m.get(&key).cloned())
}

fn any_active_project() -> Option<String> {
    ACTIVE_PROJECTS
        .lock()
        .ok()
        .and_then(|m| m.values().next().cloned())
}

fn register_active_project(req: &RequestEnvelope, project_root: &str) {
    let key = context_key_for_request(req);
    if let Ok(mut m) = ACTIVE_PROJECTS.lock() {
        m.insert(key, project_root.to_string());
    }
}

fn resolve_project_root_for_action(req: &RequestEnvelope) -> Option<String> {
    extract_root_path(&req.payload).or_else(|| active_project_for_request(req))
}

fn compute_incremental_warmup(project_root: PathBuf) -> Result<(Vec<String>, usize), String> {
    let snapshot = collect_java_file_mtimes(&project_root)?;
    let root_key = project_root.to_string_lossy().to_string();

    let mut guard = WARMUP_SNAPSHOTS
        .lock()
        .map_err(|_| "warmup snapshot lock poisoned".to_string())?;
    let previous = guard.get(&root_key);

    let changed_files = diff_changed_files(previous, &snapshot);
    let reindexed_files = changed_files.len();

    guard.insert(root_key, snapshot);
    Ok((changed_files, reindexed_files))
}

fn collect_java_file_mtimes(root: &PathBuf) -> Result<HashMap<String, u64>, String> {
    let mut out = HashMap::new();
    if !root.exists() {
        return Ok(out);
    }
    collect_java_file_mtimes_recursive(root, &mut out)?;
    Ok(out)
}

fn collect_java_file_mtimes_recursive(
    dir: &PathBuf,
    out: &mut HashMap<String, u64>,
) -> Result<(), String> {
    let entries =
        fs::read_dir(dir).map_err(|e| format!("read_dir failed for {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("read_dir entry failed: {e}"))?;
        let path = entry.path();
        if path.is_dir() {
            collect_java_file_mtimes_recursive(&path, out)?;
            continue;
        }
        let is_java = path
            .extension()
            .and_then(|x| x.to_str())
            .map(|x| x.eq_ignore_ascii_case("java"))
            .unwrap_or(false);
        if !is_java {
            continue;
        }
        let modified_secs = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or_else(current_unix_secs);
        out.insert(path.to_string_lossy().to_string(), modified_secs);
    }
    Ok(())
}

fn diff_changed_files(
    previous: Option<&HashMap<String, u64>>,
    current: &HashMap<String, u64>,
) -> Vec<String> {
    let mut changed: Vec<String> = Vec::new();
    match previous {
        None => {
            changed.extend(current.keys().cloned());
        }
        Some(prev) => {
            for (path, ts) in current {
                if prev.get(path) != Some(ts) {
                    changed.push(path.clone());
                }
            }
            for path in prev.keys() {
                if !current.contains_key(path) {
                    changed.push(path.clone());
                }
            }
        }
    }
    changed.sort();
    changed
}

fn current_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn ok_response(req: RequestEnvelope, data: Value) -> ResponseEnvelope {
    ResponseEnvelope {
        version: "v0".to_string(),
        request_id: req.request_id,
        trace_id: req.trace_id,
        status: "ok".to_string(),
        data: Some(data),
        error: None,
        fallback_used: false,
        cost: json!({ "duration_ms": 1 }),
    }
}

fn bad_request_response(req: RequestEnvelope, message: &str) -> ResponseEnvelope {
    error_response(
        req,
        "COMMON_BAD_REQUEST",
        message,
        false,
        "Adapter",
        json!({}),
    )
}

fn internal_error_response(req: RequestEnvelope, code: &str, message: String) -> ResponseEnvelope {
    error_response(req, code, &message, true, "Adapter", json!({}))
}

fn error_response(
    req: RequestEnvelope,
    code: &str,
    message: &str,
    retryable: bool,
    node: &str,
    details: Value,
) -> ResponseEnvelope {
    ResponseEnvelope {
        version: "v0".to_string(),
        request_id: req.request_id,
        trace_id: req.trace_id,
        status: "error".to_string(),
        data: None,
        error: Some(ErrorBody {
            code: code.to_string(),
            message: message.to_string(),
            retryable,
            node: node.to_string(),
            details,
        }),
        fallback_used: false,
        cost: json!({ "duration_ms": 0 }),
    }
}

fn precondition_required_response(req: RequestEnvelope, action: &str) -> ResponseEnvelope {
    error_response(
        req,
        "COMMON_PRECONDITION_REQUIRED",
        "method requires warmup before use",
        false,
        "Adapter",
        json!({
            "action": action,
            "next_action": "system.warmup",
            "example": {
                "action": "system.warmup",
                "payload": {"project_root": "<project-root>"}
            }
        }),
    )
}

fn project_activation_required_response(req: RequestEnvelope, action: &str) -> ResponseEnvelope {
    error_response(
        req,
        "COMMON_PRECONDITION_REQUIRED",
        "method requires active project context",
        false,
        "Adapter",
        json!({
            "action": action,
            "next_action": "project.activate",
            "example": {
                "action": "project.activate",
                "payload": {"project_root": "<project-root>"}
            }
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::Path;
    use std::sync::Mutex;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn req(action: &str) -> RequestEnvelope {
        RequestEnvelope {
            version: "v0".to_string(),
            request_id: "req_001".to_string(),
            trace_id: "tr_001".to_string(),
            session_id: None,
            source: "test".to_string(),
            target: "adapter".to_string(),
            action: action.to_string(),
            payload: json!({}),
            meta: None,
        }
    }

    fn clear_warmup_snapshots() {
        if let Ok(mut guard) = WARMUP_SNAPSHOTS.lock() {
            guard.clear();
        }
    }

    fn clear_active_projects() {
        if let Ok(mut guard) = ACTIVE_PROJECTS.lock() {
            guard.clear();
        }
    }

    #[test]
    fn system_health_returns_method_catalog_with_warmup_flags() {
        let _guard = TEST_LOCK.lock().expect("test lock poisoned");
        WARMED_UP.store(false, Ordering::SeqCst);
        clear_warmup_snapshots();
        clear_active_projects();
        let resp = route_action(req("system.health"));
        assert_eq!(resp.status, "ok");
        let data = resp.data.expect("system.health should return data");
        let methods = data
            .get("methods")
            .and_then(|v| v.as_array())
            .expect("methods must be array");
        assert!(!methods.is_empty());
        let symbol_lookup = methods
            .iter()
            .find(|m| m.get("action") == Some(&json!("symbol.lookup")))
            .expect("symbol.lookup method missing");
        assert_eq!(
            symbol_lookup
                .get("requires_warmup")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            true
        );
        let patch_apply = methods
            .iter()
            .find(|m| m.get("action") == Some(&json!("patch.apply")))
            .expect("patch.apply method missing");
        assert_eq!(
            patch_apply
                .get("requires_warmup")
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
            false
        );
        let project_activate = methods
            .iter()
            .find(|m| m.get("action") == Some(&json!("project.activate")))
            .expect("project.activate method missing");
        assert_eq!(
            project_activate
                .get("requires_project_activation")
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
            false
        );
    }

    #[test]
    fn symbol_lookup_requires_warmup_before_use() {
        let _guard = TEST_LOCK.lock().expect("test lock poisoned");
        WARMED_UP.store(false, Ordering::SeqCst);
        clear_warmup_snapshots();
        clear_active_projects();
        let mut lookup_req = req("symbol.lookup");
        lookup_req.payload = json!({
            "path": ".",
            "symbol": "normalizeName"
        });
        let resp = route_action(lookup_req);
        assert_eq!(resp.status, "error");
        let err = resp.error.expect("error body expected");
        assert_eq!(err.code, "COMMON_PRECONDITION_REQUIRED");
        assert_eq!(
            err.details.get("next_action").and_then(|v| v.as_str()),
            Some("system.warmup")
        );
    }

    #[test]
    fn symbol_lookup_after_warmup_is_not_blocked_by_precondition() {
        let _guard = TEST_LOCK.lock().expect("test lock poisoned");
        WARMED_UP.store(false, Ordering::SeqCst);
        clear_warmup_snapshots();
        clear_active_projects();
        let mut warmup_req = req("system.warmup");
        warmup_req.payload = json!({"project_root": "."});
        let warmup_resp = route_action(warmup_req);
        assert_eq!(warmup_resp.status, "ok");

        let mut lookup_req = req("symbol.lookup");
        lookup_req.payload = json!({
            "path": ".",
            "symbol": "normalizeName"
        });
        let resp = route_action(lookup_req);
        assert!(
            resp.error.as_ref().map(|e| e.code.as_str()) != Some("COMMON_PRECONDITION_REQUIRED")
        );
    }

    #[test]
    fn fuzzy_lookup_requires_warmup_before_use() {
        let _guard = TEST_LOCK.lock().expect("test lock poisoned");
        WARMED_UP.store(false, Ordering::SeqCst);
        clear_warmup_snapshots();
        clear_active_projects();
        let mut lookup_req = req("symbol.lookup.fuzzy");
        lookup_req.payload = json!({
            "path": ".",
            "symbol": "Name"
        });
        let resp = route_action(lookup_req);
        assert_eq!(resp.status, "error");
        let err = resp.error.expect("error body expected");
        assert_eq!(err.code, "COMMON_PRECONDITION_REQUIRED");
    }

    #[test]
    fn fuzzy_lookup_after_warmup_returns_ranked_items() {
        let _guard = TEST_LOCK.lock().expect("test lock poisoned");
        WARMED_UP.store(false, Ordering::SeqCst);
        clear_warmup_snapshots();
        clear_active_projects();

        let root = Path::new("../../..").join("fixtures").join("java-sample");
        let mut warmup_req = req("system.warmup");
        warmup_req.payload = json!({"project_root": root.to_string_lossy().to_string()});
        let warmup_resp = route_action(warmup_req);
        assert_eq!(warmup_resp.status, "ok");

        let mut lookup_req = req("symbol.lookup.fuzzy");
        lookup_req.payload = json!({
            "project_root": root.to_string_lossy().to_string(),
            "symbol": "Name",
            "limit": 10
        });
        let resp = route_action(lookup_req);
        assert_eq!(resp.status, "ok");
        let data = resp.data.expect("fuzzy data expected");
        let items = data
            .get("items")
            .and_then(|v| v.as_array())
            .expect("items should be array");
        assert!(!items.is_empty());
        let first = items.first().expect("first item");
        assert!(first.get("score").is_some());
        assert!(first.get("match_type").is_some());
    }

    #[test]
    fn graph_calls_requires_warmup_before_use() {
        let _guard = TEST_LOCK.lock().expect("test lock poisoned");
        WARMED_UP.store(false, Ordering::SeqCst);
        clear_warmup_snapshots();
        clear_active_projects();
        let mut req_graph = req("graph.calls");
        req_graph.payload = json!({"symbol": "normalizeName"});
        let resp = route_action(req_graph);
        assert_eq!(resp.status, "error");
        let err = resp.error.expect("error body expected");
        assert_eq!(err.code, "COMMON_PRECONDITION_REQUIRED");
    }

    #[test]
    fn graph_calls_after_warmup_returns_ok_shape() {
        let _guard = TEST_LOCK.lock().expect("test lock poisoned");
        WARMED_UP.store(false, Ordering::SeqCst);
        clear_warmup_snapshots();
        clear_active_projects();
        let mut warmup_req = req("system.warmup");
        warmup_req.payload = json!({"project_root": "."});
        let warmup_resp = route_action(warmup_req);
        assert_eq!(warmup_resp.status, "ok");

        let mut req_graph = req("graph.calls");
        req_graph.payload = json!({"symbol": "normalizeName", "depth": 2, "language": "java"});
        let resp = route_action(req_graph);
        assert_eq!(resp.status, "ok");
        let data = resp.data.expect("graph.calls data expected");
        assert_eq!(
            data.get("provider").and_then(|v| v.as_str()),
            Some("alsp.graph.calls.v0")
        );
    }

    #[test]
    fn warmup_incremental_reports_only_changed_java_files() {
        let _guard = TEST_LOCK.lock().expect("test lock poisoned");
        WARMED_UP.store(false, Ordering::SeqCst);
        clear_warmup_snapshots();
        clear_active_projects();

        let root =
            std::env::temp_dir().join(format!("lesscoder_warmup_test_{}", current_unix_secs()));
        fs::create_dir_all(&root).expect("create temp root");
        let java_file = root.join("A.java");
        let mut f = fs::File::create(&java_file).expect("create java file");
        writeln!(f, "class A {{}}").expect("write initial java file");

        let mut warmup_1 = req("system.warmup");
        warmup_1.payload = json!({"project_root": root.to_string_lossy().to_string()});
        let resp1 = route_action(warmup_1);
        assert_eq!(resp1.status, "ok");
        let data1 = resp1.data.expect("warmup data expected");
        let changed_1 = data1
            .get("changed_files")
            .and_then(|v| v.as_array())
            .expect("changed_files must be array");
        assert!(!changed_1.is_empty());

        let mut warmup_2 = req("system.warmup");
        warmup_2.payload = json!({"project_root": root.to_string_lossy().to_string()});
        let resp2 = route_action(warmup_2);
        assert_eq!(resp2.status, "ok");
        let data2 = resp2.data.expect("warmup data expected");
        let changed_2 = data2
            .get("changed_files")
            .and_then(|v| v.as_array())
            .expect("changed_files must be array");
        assert!(changed_2.is_empty());

        std::thread::sleep(std::time::Duration::from_secs(1));
        let mut f2 = fs::File::create(&java_file).expect("reopen java file");
        writeln!(f2, "class A {{ int x = 1; }}").expect("rewrite java file");

        let mut warmup_3 = req("system.warmup");
        warmup_3.payload = json!({"project_root": root.to_string_lossy().to_string()});
        let resp3 = route_action(warmup_3);
        assert_eq!(resp3.status, "ok");
        let data3 = resp3.data.expect("warmup data expected");
        let changed_3 = data3
            .get("changed_files")
            .and_then(|v| v.as_array())
            .expect("changed_files must be array");
        assert_eq!(changed_3.len(), 1);
        assert_eq!(
            data3
                .get("reindexed_files")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            1
        );

        let _ = fs::remove_file(&java_file);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn symbol_lookup_requires_active_project_when_no_root_provided() {
        let _guard = TEST_LOCK.lock().expect("test lock poisoned");
        WARMED_UP.store(true, Ordering::SeqCst);
        clear_warmup_snapshots();
        clear_active_projects();

        let mut lookup_req = req("symbol.lookup");
        lookup_req.payload = json!({"symbol": "normalizeName"});
        let resp = route_action(lookup_req);
        assert_eq!(resp.status, "error");
        let err = resp.error.expect("error body expected");
        assert_eq!(err.code, "COMMON_PRECONDITION_REQUIRED");
        assert_eq!(
            err.details.get("next_action").and_then(|v| v.as_str()),
            Some("project.activate")
        );
    }

    #[test]
    fn project_activate_sets_context_for_followup_symbol_lookup() {
        let _guard = TEST_LOCK.lock().expect("test lock poisoned");
        WARMED_UP.store(true, Ordering::SeqCst);
        clear_warmup_snapshots();
        clear_active_projects();

        let root = Path::new("../../..").join("fixtures").join("java-sample");
        let root_str = root.to_string_lossy().to_string();

        let mut activate_req = req("project.activate");
        activate_req.payload = json!({"project_root": root_str});
        let activate_resp = route_action(activate_req);
        assert_eq!(activate_resp.status, "ok");

        let mut lookup_req = req("symbol.lookup");
        lookup_req.payload = json!({"symbol": "normalizeName"});
        let resp = route_action(lookup_req);
        assert_eq!(resp.status, "ok");
    }

    #[test]
    fn http_health_endpoint_returns_ok() {
        let _guard = TEST_LOCK.lock().expect("test lock poisoned");
        let (status, body) = http_response_body("/health");
        assert_eq!(status, 200);
        let payload: Value = serde_json::from_str(&body).expect("valid json");
        assert_eq!(payload.get("status").and_then(|v| v.as_str()), Some("ok"));
    }

    #[test]
    fn http_methods_endpoint_returns_method_list() {
        let _guard = TEST_LOCK.lock().expect("test lock poisoned");
        let (status, body) = http_response_body("/methods");
        assert_eq!(status, 200);
        let payload: Value = serde_json::from_str(&body).expect("valid json");
        assert!(payload
            .get("methods")
            .and_then(|v| v.as_array())
            .is_some_and(|arr| !arr.is_empty()));
    }

    #[test]
    fn http_unknown_endpoint_returns_404() {
        let _guard = TEST_LOCK.lock().expect("test lock poisoned");
        let (status, body) = http_response_body("/unknown");
        assert_eq!(status, 404);
        let payload: Value = serde_json::from_str(&body).expect("valid json");
        assert_eq!(
            payload.get("error_code").and_then(|v| v.as_str()),
            Some("HTTP_NOT_FOUND")
        );
    }
}
