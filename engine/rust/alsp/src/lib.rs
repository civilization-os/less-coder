use anyhow::Result;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{LazyLock, Mutex};
use std::time::UNIX_EPOCH;
use walkdir::WalkDir;

#[derive(Debug, Serialize)]
pub struct RepoMap {
    pub root: String,
    pub language: String,
    pub files: Vec<FileMap>,
}

#[derive(Debug, Serialize, Clone)]
pub struct SymbolLocation {
    pub symbol: String,
    pub file: String,
    pub line: usize,
    pub signature: String,
    pub kind: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct FuzzySymbolMatch {
    pub symbol: String,
    pub file: String,
    pub line: usize,
    pub signature: String,
    pub kind: String,
    pub score: i32,
    pub match_type: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct GraphCallEdge {
    pub caller: String,
    pub callee: String,
    pub file: String,
    pub line: usize,
}

#[derive(Debug, Serialize, Clone)]
pub struct GraphCallsResult {
    pub symbol: String,
    pub depth: usize,
    pub provider: String,
    pub calls: Vec<GraphCallEdge>,
    pub truncated: bool,
}

#[derive(Debug, Clone)]
struct TsGraphCacheEntry {
    signature: u128,
    result: GraphCallsResult,
}

static TS_GRAPH_CACHE: LazyLock<Mutex<HashMap<String, TsGraphCacheEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct JavaFileCache {
    pub path: String,
    pub modified_unix_ms: u128,
    pub size_bytes: u64,
    pub classes: Vec<ClassSymbol>,
    pub methods: Vec<MethodSymbol>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct JavaRepoIndexCache {
    pub files: Vec<JavaFileCache>,
}

#[derive(Debug, Serialize, Clone)]
pub struct IncrementalBuildStats {
    pub scanned_files: usize,
    pub reused_files: usize,
    pub rebuilt_files: usize,
}

#[derive(Debug, Serialize)]
pub struct IncrementalRepoMapResult {
    pub map: RepoMap,
    pub cache: JavaRepoIndexCache,
    pub stats: IncrementalBuildStats,
}

#[derive(Debug, Serialize)]
pub struct FileMap {
    pub path: String,
    pub classes: Vec<ClassSymbol>,
    pub methods: Vec<MethodSymbol>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ClassSymbol {
    pub name: String,
    pub signature: String,
    pub line: usize,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MethodSymbol {
    pub name: String,
    pub signature: String,
    pub line: usize,
}

pub fn build_java_repo_map(root: &Path) -> Result<RepoMap> {
    let class_re = Regex::new(r"\b(class|interface|enum)\s+([A-Za-z_][A-Za-z0-9_]*)")?;
    // Minimal Java method signature matcher (non-perfect but sufficient for L1 baseline)
    let method_re = Regex::new(
        r"^\s*(public|protected|private)?\s*(static\s+)?([A-Za-z0-9_<>\[\], ?]+)\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(([^)]*)\)\s*\{\s*$",
    )?;

    let mut files = Vec::new();
    for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if !path.is_file() || path.extension().and_then(|s| s.to_str()) != Some("java") {
            continue;
        }

        let content = fs::read_to_string(path)?;
        let mut file_map = FileMap {
            path: to_rel_or_abs(root, path).display().to_string(),
            classes: Vec::new(),
            methods: Vec::new(),
        };

        for (idx, line) in content.lines().enumerate() {
            let lineno = idx + 1;

            if let Some(caps) = class_re.captures(line) {
                let name = caps.get(2).map(|m| m.as_str()).unwrap_or("Unknown");
                file_map.classes.push(ClassSymbol {
                    name: name.to_string(),
                    signature: line.trim().to_string(),
                    line: lineno,
                });
            }

            if let Some(caps) = method_re.captures(line) {
                let method_name = caps.get(4).map(|m| m.as_str()).unwrap_or("unknown");
                // Filter out control flow lines accidentally matching.
                if ["if", "for", "while", "switch", "catch"].contains(&method_name) {
                    continue;
                }
                file_map.methods.push(MethodSymbol {
                    name: method_name.to_string(),
                    signature: line.trim().to_string(),
                    line: lineno,
                });
            }
        }

        if !file_map.classes.is_empty() || !file_map.methods.is_empty() {
            files.push(file_map);
        }
    }

    Ok(RepoMap {
        root: root.display().to_string(),
        language: "java".to_string(),
        files,
    })
}

pub fn build_java_repo_map_incremental(
    root: &Path,
    previous: Option<&JavaRepoIndexCache>,
) -> Result<IncrementalRepoMapResult> {
    let class_re = Regex::new(r"\b(class|interface|enum)\s+([A-Za-z_][A-Za-z0-9_]*)")?;
    let method_re = Regex::new(
        r"^\s*(public|protected|private)?\s*(static\s+)?([A-Za-z0-9_<>\[\], ?]+)\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(([^)]*)\)\s*\{\s*$",
    )?;

    let mut scanned_files = 0usize;
    let mut reused_files = 0usize;
    let mut rebuilt_files = 0usize;

    let prev_map = previous
        .map(|c| {
            c.files
                .iter()
                .map(|f| (f.path.clone(), f.clone()))
                .collect::<std::collections::HashMap<_, _>>()
        })
        .unwrap_or_default();

    let mut next_cache_files: Vec<JavaFileCache> = Vec::new();
    let mut repo_files: Vec<FileMap> = Vec::new();

    for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if !path.is_file() || path.extension().and_then(|s| s.to_str()) != Some("java") {
            continue;
        }
        scanned_files += 1;
        let rel = to_rel_or_abs(root, path).display().to_string();
        let meta = fs::metadata(path)?;
        let modified_unix_ms = meta
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let size_bytes = meta.len();

        if let Some(prev) = prev_map.get(&rel) {
            if prev.modified_unix_ms == modified_unix_ms && prev.size_bytes == size_bytes {
                reused_files += 1;
                next_cache_files.push(prev.clone());
                repo_files.push(FileMap {
                    path: prev.path.clone(),
                    classes: prev.classes.clone(),
                    methods: prev.methods.clone(),
                });
                continue;
            }
        }

        rebuilt_files += 1;
        let parsed = parse_java_file(path, root, &class_re, &method_re)?;
        next_cache_files.push(JavaFileCache {
            path: parsed.path.clone(),
            modified_unix_ms,
            size_bytes,
            classes: parsed.classes.clone(),
            methods: parsed.methods.clone(),
        });
        repo_files.push(parsed);
    }

    let cache = JavaRepoIndexCache {
        files: next_cache_files,
    };
    let map = RepoMap {
        root: root.display().to_string(),
        language: "java".to_string(),
        files: repo_files,
    };
    let stats = IncrementalBuildStats {
        scanned_files,
        reused_files,
        rebuilt_files,
    };

    Ok(IncrementalRepoMapResult { map, cache, stats })
}

pub fn lookup_java_symbol(root: &Path, symbol: &str) -> Result<Option<SymbolLocation>> {
    let map = build_java_repo_map(root)?;
    for file in map.files {
        for cls in &file.classes {
            if cls.name == symbol {
                return Ok(Some(SymbolLocation {
                    symbol: symbol.to_string(),
                    file: file.path.clone(),
                    line: cls.line,
                    signature: cls.signature.clone(),
                    kind: "class".to_string(),
                }));
            }
        }
        for method in &file.methods {
            if method.name == symbol {
                return Ok(Some(SymbolLocation {
                    symbol: symbol.to_string(),
                    file: file.path.clone(),
                    line: method.line,
                    signature: method.signature.clone(),
                    kind: "method".to_string(),
                }));
            }
        }
    }
    Ok(None)
}

pub fn lookup_java_symbol_fuzzy(
    root: &Path,
    symbol: &str,
    limit: usize,
) -> Result<Vec<FuzzySymbolMatch>> {
    let needle = symbol.trim().to_lowercase();
    if needle.is_empty() {
        return Ok(Vec::new());
    }

    let map = build_java_repo_map(root)?;
    let mut out: Vec<FuzzySymbolMatch> = Vec::new();
    let mut dedup: HashSet<(String, String, String)> = HashSet::new();

    for file in map.files {
        for cls in &file.classes {
            if let Some((score, match_type)) = score_symbol_match(&cls.name, &needle) {
                let key = (file.path.clone(), cls.name.clone(), "class".to_string());
                if dedup.insert(key) {
                    out.push(FuzzySymbolMatch {
                        symbol: cls.name.clone(),
                        file: file.path.clone(),
                        line: cls.line,
                        signature: cls.signature.clone(),
                        kind: "class".to_string(),
                        score,
                        match_type,
                    });
                }
            }
        }
        for method in &file.methods {
            if let Some((score, match_type)) = score_symbol_match(&method.name, &needle) {
                let key = (file.path.clone(), method.name.clone(), "method".to_string());
                if dedup.insert(key) {
                    out.push(FuzzySymbolMatch {
                        symbol: method.name.clone(),
                        file: file.path.clone(),
                        line: method.line,
                        signature: method.signature.clone(),
                        kind: "method".to_string(),
                        score,
                        match_type,
                    });
                }
            }
        }
    }

    out.sort_by(stable_fuzzy_ordering);
    let capped = if limit == 0 { 0 } else { limit };
    if out.len() > capped {
        out.truncate(capped);
    }
    Ok(out)
}

fn score_symbol_match(candidate: &str, needle_lower: &str) -> Option<(i32, String)> {
    let c_lower = candidate.to_lowercase();
    let n_len = needle_lower.len() as i32;
    let c_len = c_lower.len() as i32;
    let len_delta = (c_len - n_len).abs();
    if c_lower.starts_with(needle_lower) {
        let score = 10_000 - len_delta;
        return Some((score, "prefix".to_string()));
    }
    if c_lower.contains(needle_lower) {
        let score = 1_000 - len_delta;
        return Some((score, "contains".to_string()));
    }
    None
}

fn stable_fuzzy_ordering(a: &FuzzySymbolMatch, b: &FuzzySymbolMatch) -> Ordering {
    b.score
        .cmp(&a.score)
        .then_with(|| a.symbol.cmp(&b.symbol))
        .then_with(|| a.file.cmp(&b.file))
        .then_with(|| a.line.cmp(&b.line))
}

pub fn graph_calls_typescript_lsp(
    root: &Path,
    symbol: &str,
    depth: usize,
) -> Result<GraphCallsResult> {
    let sanitized_symbol = symbol.trim();
    if sanitized_symbol.is_empty() {
        return Ok(GraphCallsResult {
            symbol: String::new(),
            depth,
            provider: "ts.lsp.calls.v1".to_string(),
            calls: Vec::new(),
            truncated: false,
        });
    }

    let signature = compute_ts_workspace_signature(root);
    let cache_key = format!("{}|{}|{}", root.display(), sanitized_symbol, depth);
    if let Ok(cache) = TS_GRAPH_CACHE.lock() {
        if let Some(hit) = cache.get(&cache_key) {
            if hit.signature == signature {
                return Ok(hit.result.clone());
            }
        }
    }

    let result = run_ts_lsp_graph_calls(root, sanitized_symbol, depth)?;
    if let Ok(mut cache) = TS_GRAPH_CACHE.lock() {
        cache.insert(
            cache_key,
            TsGraphCacheEntry {
                signature,
                result: result.clone(),
            },
        );
    }
    Ok(result)
}

fn run_ts_lsp_graph_calls(root: &Path, symbol: &str, depth: usize) -> Result<GraphCallsResult> {
    if !workspace_has_ts_files(root) {
        return Ok(GraphCallsResult {
            symbol: symbol.to_string(),
            depth,
            provider: "ts.lsp.calls.v1".to_string(),
            calls: Vec::new(),
            truncated: depth > 1,
        });
    }

    let mut client = SimpleLspClient::start(root)?;
    client.initialize(root)?;

    let symbols = client.workspace_symbol(symbol)?;
    let selected = symbols.into_iter().max_by(|a, b| {
        a.score
            .cmp(&b.score)
            .then_with(|| b.name.len().cmp(&a.name.len()))
    });
    let Some(target) = selected else {
        return Ok(GraphCallsResult {
            symbol: symbol.to_string(),
            depth,
            provider: "ts.lsp.calls.v1".to_string(),
            calls: Vec::new(),
            truncated: depth > 1,
        });
    };

    let refs = client.references(&target.uri, target.line, target.character)?;
    let mut dedup: HashSet<(String, String, usize)> = HashSet::new();
    let mut calls: Vec<GraphCallEdge> = Vec::new();
    for r in refs {
        let path = file_uri_to_path(&r.uri);
        let caller =
            find_enclosing_ts_symbol(&path, r.line).unwrap_or_else(|| "<module>".to_string());
        let file_display = to_rel_or_abs(root, &path).display().to_string();
        if dedup.insert((caller.clone(), file_display.clone(), r.line + 1)) {
            calls.push(GraphCallEdge {
                caller,
                callee: target.name.clone(),
                file: file_display,
                line: r.line + 1,
            });
        }
    }
    calls.sort_by(|a, b| {
        a.caller
            .cmp(&b.caller)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
    });

    Ok(GraphCallsResult {
        symbol: target.name,
        depth,
        provider: "ts.lsp.calls.v1".to_string(),
        calls,
        truncated: depth > 1,
    })
}

fn workspace_has_ts_files(root: &Path) -> bool {
    for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        if matches!(
            p.extension().and_then(|x| x.to_str()),
            Some("ts") | Some("tsx") | Some("js") | Some("jsx")
        ) {
            return true;
        }
    }
    false
}

fn compute_ts_workspace_signature(root: &Path) -> u128 {
    let mut acc: u128 = 14695981039346656037u128;
    for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        if !matches!(
            p.extension().and_then(|x| x.to_str()),
            Some("ts") | Some("tsx") | Some("js") | Some("jsx")
        ) {
            continue;
        }
        if let Ok(meta) = fs::metadata(p) {
            let modified = meta
                .modified()
                .ok()
                .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis())
                .unwrap_or(0);
            acc ^= modified.wrapping_mul(1099511628211u128);
            acc = acc.rotate_left(13) ^ (meta.len() as u128);
        }
    }
    acc
}

#[derive(Debug, Clone)]
struct WorkspaceSymbolCandidate {
    name: String,
    uri: String,
    line: usize,
    character: usize,
    score: i32,
}

#[derive(Debug, Clone)]
struct LspLocation {
    uri: String,
    line: usize,
    _character: usize,
}

struct SimpleLspClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl SimpleLspClient {
    fn start(root: &Path) -> Result<Self> {
        let mut cmd = Command::new("typescript-language-server");
        cmd.arg("--stdio")
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = cmd.spawn().map_err(|e| {
            anyhow::anyhow!(
                "LSP_UNAVAILABLE: failed to spawn typescript-language-server (install with npm i -g typescript-language-server typescript): {e}"
            )
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("LSP_INTERNAL: missing stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("LSP_INTERNAL: missing stdout"))?;
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
        })
    }

    fn initialize(&mut self, root: &Path) -> Result<()> {
        let root_uri = path_to_file_uri(root);
        let _ = self.request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": root_uri,
                "capabilities": {},
                "workspaceFolders": [
                    {"uri": root_uri, "name": root.file_name().and_then(|v| v.to_str()).unwrap_or("workspace")}
                ]
            }),
        )?;
        self.notification("initialized", json!({}))?;
        Ok(())
    }

    fn workspace_symbol(&mut self, query: &str) -> Result<Vec<WorkspaceSymbolCandidate>> {
        let resp = self.request("workspace/symbol", json!({ "query": query }))?;
        let mut out = Vec::new();
        let arr = resp.as_array().cloned().unwrap_or_default();
        for item in arr {
            let name = item
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let uri = item
                .get("location")
                .and_then(|v| v.get("uri"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let line = item
                .get("location")
                .and_then(|v| v.get("range"))
                .and_then(|v| v.get("start"))
                .and_then(|v| v.get("line"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            let character = item
                .get("location")
                .and_then(|v| v.get("range"))
                .and_then(|v| v.get("start"))
                .and_then(|v| v.get("character"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            if name.is_empty() || uri.is_empty() {
                continue;
            }
            let score = compute_symbol_query_score(&name, query);
            if score <= 0 {
                continue;
            }
            out.push(WorkspaceSymbolCandidate {
                name,
                uri,
                line,
                character,
                score,
            });
        }
        Ok(out)
    }

    fn references(&mut self, uri: &str, line: usize, character: usize) -> Result<Vec<LspLocation>> {
        let resp = self.request(
            "textDocument/references",
            json!({
                "textDocument": {"uri": uri},
                "position": {"line": line, "character": character},
                "context": {"includeDeclaration": false}
            }),
        )?;
        let mut out = Vec::new();
        let arr = resp.as_array().cloned().unwrap_or_default();
        for item in arr {
            let loc_uri = item
                .get("uri")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let loc_line = item
                .get("range")
                .and_then(|v| v.get("start"))
                .and_then(|v| v.get("line"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            let loc_char = item
                .get("range")
                .and_then(|v| v.get("start"))
                .and_then(|v| v.get("character"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            if !loc_uri.is_empty() {
                out.push(LspLocation {
                    uri: loc_uri,
                    line: loc_line,
                    _character: loc_char,
                });
            }
        }
        Ok(out)
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.send_json(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }))?;
        loop {
            let msg = self.read_json()?;
            if msg.get("id").and_then(|v| v.as_u64()) == Some(id) {
                if let Some(err) = msg.get("error") {
                    return Err(anyhow::anyhow!("LSP_INTERNAL: {err}"));
                }
                return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
            }
        }
    }

    fn notification(&mut self, method: &str, params: Value) -> Result<()> {
        self.send_json(json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        }))
    }

    fn send_json(&mut self, payload: Value) -> Result<()> {
        let body = serde_json::to_vec(&payload)?;
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        self.stdin.write_all(header.as_bytes())?;
        self.stdin.write_all(&body)?;
        self.stdin.flush()?;
        Ok(())
    }

    fn read_json(&mut self) -> Result<Value> {
        let mut content_length: usize = 0;
        loop {
            let mut line = String::new();
            let n = self.stdout.read_line(&mut line)?;
            if n == 0 {
                return Err(anyhow::anyhow!(
                    "LSP_TIMEOUT: unexpected EOF from language server"
                ));
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                break;
            }
            let lower = trimmed.to_ascii_lowercase();
            if let Some(rest) = lower.strip_prefix("content-length:") {
                content_length = rest.trim().parse::<usize>().unwrap_or(0);
            }
        }
        if content_length == 0 {
            return Err(anyhow::anyhow!("LSP_INTERNAL: invalid content-length"));
        }
        let mut buf = vec![0u8; content_length];
        self.stdout.read_exact(&mut buf)?;
        let value: Value = serde_json::from_slice(&buf)?;
        Ok(value)
    }
}

impl Drop for SimpleLspClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn compute_symbol_query_score(name: &str, query: &str) -> i32 {
    let n = name.to_lowercase();
    let q = query.to_lowercase();
    let len_delta = (n.len() as i32 - q.len() as i32).abs();
    if n == q {
        return 20_000 - len_delta;
    }
    if n.starts_with(&q) {
        return 10_000 - len_delta;
    }
    if n.contains(&q) {
        return 1_000 - len_delta;
    }
    0
}

fn find_enclosing_ts_symbol(path: &Path, zero_based_line: usize) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    let function_re =
        Regex::new(r"^\s*(export\s+)?(async\s+)?function\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(").ok()?;
    let arrow_re = Regex::new(r"^\s*(export\s+)?(const|let|var)\s+([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(async\s*)?\([^)]*\)\s*=>").ok()?;
    let method_re = Regex::new(
        r"^\s*(public|private|protected)?\s*(async\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*\([^)]*\)\s*\{",
    )
    .ok()?;
    let mut last = None;
    for (idx, line) in content.lines().enumerate() {
        if idx > zero_based_line {
            break;
        }
        if let Some(c) = function_re.captures(line) {
            last = c.get(3).map(|m| m.as_str().to_string());
            continue;
        }
        if let Some(c) = arrow_re.captures(line) {
            last = c.get(3).map(|m| m.as_str().to_string());
            continue;
        }
        if let Some(c) = method_re.captures(line) {
            if let Some(name) = c.get(3).map(|m| m.as_str().to_string()) {
                if !["if", "for", "while", "switch", "catch"].contains(&name.as_str()) {
                    last = Some(name);
                }
            }
        }
    }
    last
}

fn path_to_file_uri(path: &Path) -> String {
    let abs = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let mut s = abs.to_string_lossy().replace('\\', "/");
    if !s.starts_with('/') {
        s = format!("/{}", s);
    }
    format!("file://{}", s)
}

fn file_uri_to_path(uri: &str) -> PathBuf {
    let mut s = uri.trim();
    if let Some(rest) = s.strip_prefix("file://") {
        s = rest;
    }
    let decoded = percent_decode(s);
    #[cfg(windows)]
    {
        let mut out = decoded;
        if out.starts_with('/') && out.len() > 2 && out.as_bytes()[2] == b':' {
            out = out[1..].to_string();
        }
        return PathBuf::from(out.replace('/', "\\"));
    }
    #[cfg(not(windows))]
    {
        PathBuf::from(decoded)
    }
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let h1 = bytes[i + 1] as char;
            let h2 = bytes[i + 2] as char;
            if h1.is_ascii_hexdigit() && h2.is_ascii_hexdigit() {
                let v = u8::from_str_radix(&format!("{h1}{h2}"), 16).unwrap_or(b'?');
                out.push(v as char);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn to_rel_or_abs(root: &Path, p: &Path) -> PathBuf {
    p.strip_prefix(root)
        .map(|v| v.to_path_buf())
        .unwrap_or_else(|_| p.to_path_buf())
}

fn parse_java_file(
    path: &Path,
    root: &Path,
    class_re: &Regex,
    method_re: &Regex,
) -> Result<FileMap> {
    let content = fs::read_to_string(path)?;
    let mut file_map = FileMap {
        path: to_rel_or_abs(root, path).display().to_string(),
        classes: Vec::new(),
        methods: Vec::new(),
    };
    for (idx, line) in content.lines().enumerate() {
        let lineno = idx + 1;
        if let Some(caps) = class_re.captures(line) {
            let name = caps.get(2).map(|m| m.as_str()).unwrap_or("Unknown");
            file_map.classes.push(ClassSymbol {
                name: name.to_string(),
                signature: line.trim().to_string(),
                line: lineno,
            });
        }
        if let Some(caps) = method_re.captures(line) {
            let method_name = caps.get(4).map(|m| m.as_str()).unwrap_or("unknown");
            if ["if", "for", "while", "switch", "catch"].contains(&method_name) {
                continue;
            }
            file_map.methods.push(MethodSymbol {
                name: method_name.to_string(),
                signature: line.trim().to_string(),
                line: lineno,
            });
        }
    }
    Ok(file_map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn should_build_repo_map_for_java_fixture() {
        let root = Path::new("../../..").join("fixtures").join("java-sample");
        let map = build_java_repo_map(&root).expect("repo map build failed");
        assert!(!map.files.is_empty());
        let joined = serde_json::to_string(&map).expect("json");
        assert!(joined.contains("NameService"));
        assert!(joined.contains("normalizeName"));
    }

    #[test]
    fn should_lookup_method_symbol_for_java_fixture() {
        let root = Path::new("../../..").join("fixtures").join("java-sample");
        let hit = lookup_java_symbol(&root, "normalizeName")
            .expect("lookup should not fail")
            .expect("symbol should exist");
        assert_eq!(hit.kind, "method");
        assert_eq!(hit.file, r"src\main\java\com\acme\NameService.java");
        assert_eq!(hit.line, 4);
    }

    #[test]
    fn should_lookup_class_symbol_for_java_fixture() {
        let root = Path::new("../../..").join("fixtures").join("java-sample");
        let hit = lookup_java_symbol(&root, "NameService")
            .expect("lookup should not fail")
            .expect("symbol should exist");
        assert_eq!(hit.kind, "class");
        assert_eq!(hit.file, r"src\main\java\com\acme\NameService.java");
        assert_eq!(hit.line, 3);
    }

    #[test]
    fn should_return_none_for_unknown_symbol() {
        let root = Path::new("../../..").join("fixtures").join("java-sample");
        let hit = lookup_java_symbol(&root, "NotExistingSymbol").expect("lookup should not fail");
        assert!(hit.is_none());
    }

    #[test]
    fn should_reuse_unchanged_files_in_incremental_mode() {
        let root = Path::new("../../..").join("fixtures").join("java-sample");
        let first = build_java_repo_map_incremental(&root, None).expect("first build");
        assert!(first.stats.rebuilt_files > 0);
        let second =
            build_java_repo_map_incremental(&root, Some(&first.cache)).expect("second build");
        assert_eq!(second.stats.rebuilt_files, 0);
        assert!(second.stats.reused_files >= 2);
    }

    #[test]
    fn should_rebuild_changed_file_in_incremental_mode() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("java-sample");
        std::fs::create_dir_all(root.join("src/main/java/com/acme")).expect("mkdir");

        let target = root.join("src/main/java/com/acme/NameService.java");
        std::fs::write(
            &target,
            "package com.acme;\npublic class NameService {\npublic String normalizeName(String input) {\nreturn input;\n}\n}\n",
        )
        .expect("write");

        let first = build_java_repo_map_incremental(&root, None).expect("first build");
        assert_eq!(first.stats.rebuilt_files, 1);

        // Ensure fs mtime granularity difference before rewrite.
        thread::sleep(Duration::from_millis(20));
        std::fs::write(
            &target,
            "package com.acme;\npublic class NameService {\npublic String normalizeName(String input) {\nreturn input.trim();\n}\n}\n",
        )
        .expect("rewrite");

        let second =
            build_java_repo_map_incremental(&root, Some(&first.cache)).expect("second build");
        assert_eq!(second.stats.rebuilt_files, 1);
        assert_eq!(second.stats.reused_files, 0);
    }

    #[test]
    fn fuzzy_lookup_should_prioritize_prefix_over_contains() {
        let root = Path::new("../../..").join("fixtures").join("java-sample");
        let hits = lookup_java_symbol_fuzzy(&root, "Name", 10).expect("fuzzy lookup");
        assert!(!hits.is_empty());
        assert_eq!(hits[0].symbol, "NameService");
        assert_eq!(hits[0].match_type, "prefix");
    }

    #[test]
    fn fuzzy_lookup_should_apply_limit_and_stable_sort() {
        let root = Path::new("../../..").join("fixtures").join("java-sample");
        let first = lookup_java_symbol_fuzzy(&root, "Name", 1).expect("first fuzzy");
        let second = lookup_java_symbol_fuzzy(&root, "Name", 1).expect("second fuzzy");
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert_eq!(first[0].symbol, second[0].symbol);
        assert_eq!(first[0].file, second[0].file);
    }

    #[test]
    fn fuzzy_lookup_should_return_empty_for_unknown_symbol() {
        let root = Path::new("../../..").join("fixtures").join("java-sample");
        let hits =
            lookup_java_symbol_fuzzy(&root, "TotallyUnknownKeyword", 10).expect("fuzzy lookup");
        assert!(hits.is_empty());
    }

    #[test]
    fn ts_graph_calls_returns_empty_when_workspace_has_no_ts_files() {
        let root = Path::new("../../..").join("fixtures").join("java-sample");
        let res = graph_calls_typescript_lsp(&root, "normalizeName", 2).expect("ts graph calls");
        assert_eq!(res.provider, "ts.lsp.calls.v1");
        assert!(res.calls.is_empty());
        assert!(res.truncated);
    }

    #[test]
    fn ts_graph_calls_handles_missing_or_available_lsp() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("ts-sample");
        std::fs::create_dir_all(&root).expect("mkdir");
        std::fs::write(
            root.join("index.ts"),
            "export function greet(name: string) { return name.trim(); }\n",
        )
        .expect("write");

        let res = graph_calls_typescript_lsp(&root, "greet", 1);
        match res {
            Ok(v) => {
                assert_eq!(v.provider, "ts.lsp.calls.v1");
            }
            Err(err) => {
                let text = err.to_string();
                assert!(
                    text.contains("LSP_UNAVAILABLE") || text.contains("LSP_TIMEOUT"),
                    "unexpected error: {text}"
                );
            }
        }
    }
}
