use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

const NS_UUID: &str = "6f1b2c3d-4e5f-4a6b-8c9d-0e1f2a3b4c5d";
const MIN_CHUNK: usize = 80;
const MAX_CHUNK: usize = 3600;
const MAX_FILE: u64 = 400_000;

const SKIP_DIRS: &[&str] = &[
    "target",
    "node_modules",
    "vendor",
    "dist",
    "build",
    "coverage",
    "out",
    "__pycache__",
    "Pods",
    "third_party",
    "site-packages",
];

const CONTAINERS: &[&str] = &[
    "source_file",
    "program",
    "module",
    "translation_unit",
    "impl_item",
    "mod_item",
    "class_declaration",
    "interface_declaration",
    "declaration_list",
    "class_body",
    "interface_body",
    "block",
    "export_statement",
];

struct Cfg {
    qdrant: String,
    apikey: String,
    auth_header: String,
    ollama: String,
    model: String,
    collection: String,
    repo: String,
    dry: bool,
    batch: usize,
    max_files: usize,
    skip_tests: bool,
}

struct Chunk {
    label: String,
    kind: String,
    line: usize,
    body: String,
}

fn env_or(k: &str, d: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| d.to_string())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn usage() {
    println!("gh2qdrant - clone a GitHub repo and index its code into Qdrant");
    println!("");
    println!("  gh2qdrant <github-url> [options]");
    println!("  --collection NAME   target collection (default code_<repo>)");
    println!("  --repo NAME         payload repo label (default: from url)");
    println!("  --batch N           embeddings per request (default 8)");
    println!("  --limit-files N     stop after N files");
    println!("  --keep-tests        also index cfg(test) modules");
    println!("  --dry               chunk only, write nothing");
    println!(
        "  env: QDRANT_URL QDRANT_API_KEY OLLAMA_URL EMBED_MODEL WORK_DIR"
    );
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() {
        usage();
        return;
    }
    let mut url_in = String::new();
    let mut collection: Option<String> = None;
    let mut repo_override: Option<String> = None;
    let mut batch = 8usize;
    let mut max_files = 1_000_000usize;
    let mut dry = false;
    let mut skip_tests = true;
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--collection" => {
                i += 1;
                collection = argv.get(i).cloned();
            }
            "--repo" => {
                i += 1;
                repo_override = argv.get(i).cloned();
            }
            "--batch" => {
                i += 1;
                batch = argv.get(i).and_then(|v| v.parse().ok()).unwrap_or(8);
            }
            "--limit-files" => {
                i += 1;
                max_files = argv
                    .get(i)
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(max_files);
            }
            "--dry" => dry = true,
            "--keep-tests" => skip_tests = false,
            "-h" | "--help" => {
                usage();
                return;
            }
            other => {
                if !other.starts_with("--") {
                    url_in = other.to_string();
                }
            }
        }
        i += 1;
    }
    if url_in.is_empty() {
        usage();
        return;
    }

    let (clone_url, branch, subpath) = parse_url(&url_in);
    let repo = repo_override.unwrap_or_else(|| repo_name(&clone_url));
    let col = collection.unwrap_or_else(|| {
        format!(
            "code_{}",
            repo.to_lowercase().replace(".", "_").replace("-", "_")
        )
    });

    let cfg = Cfg {
        qdrant: env_or("QDRANT_URL", "http://host.containers.internal:9090"),
        apikey: env_or("QDRANT_API_KEY", "your_secure_api_key_here"),
        auth_header: env_or("QDRANT_AUTH_HEADER", "api-key"),
        ollama: env_or("OLLAMA_URL", "http://host.containers.internal:9020"),
        model: env_or("EMBED_MODEL", "bge-m3"),
        collection: col,
        repo: repo.clone(),
        dry,
        batch,
        max_files,
        skip_tests,
    };

    let t0 = Instant::now();
    println!("repo        {}", cfg.repo);
    println!("collection  {}", cfg.collection);
    println!("clone url   {}", clone_url);

    let root = match clone_repo(&cfg, &clone_url, branch.as_deref()) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("clone failed: {}", e);
            return;
        }
    };
    println!("checkout    {} ({:?})", root.display(), t0.elapsed());

    let mut files: Vec<PathBuf> = Vec::new();
    walk(&root, &mut files);
    if let Some(sp) = &subpath {
        files.retain(|p| p.to_string_lossy().contains(sp.as_str()));
    }
    files.truncate(cfg.max_files);
    println!("files       {}", files.len());

    let mut nfiles = 0usize;
    let mut nchunks = 0usize;
    let mut created = false;

    for abs in &files {
        let rel = abs
            .strip_prefix(&root)
            .unwrap_or(abs)
            .to_string_lossy()
            .to_string();
        let fname = abs.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let lang = match lang_for(fname) {
            Some(l) => l,
            None => continue,
        };
        let text = match fs::read_to_string(abs) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let chunks = chunk_file(&rel, lang, &text, cfg.skip_tests);
        if chunks.is_empty() {
            continue;
        }
        if cfg.dry {
            println!("{:6} {:10} {}", chunks.len(), lang, rel);
            nfiles += 1;
            nchunks += chunks.len();
            continue;
        }

        let docs: Vec<String> = chunks
            .iter()
            .map(|c| doc_text(&cfg.repo, &rel, lang, c))
            .collect();
        let mut vecs: Vec<Vec<f32>> = Vec::new();
        for part in docs.chunks(cfg.batch) {
            match embed(&cfg, part) {
                Ok(mut v) => vecs.append(&mut v),
                Err(e) => {
                    eprintln!("embed failed on {}: {}", rel, e);
                    return;
                }
            }
        }
        if vecs.len() != chunks.len() {
            eprintln!(
                "vector mismatch on {}: {} vs {}",
                rel,
                vecs.len(),
                chunks.len()
            );
            continue;
        }
        if !created {
            let dim = vecs[0].len();
            println!("dense dim   {}", dim);
            if let Err(e) = ensure_collection(&cfg, dim) {
                eprintln!("collection setup failed: {}", e);
                return;
            }
            created = true;
        }

        let mut points: Vec<Value> = Vec::new();
        for (c, v) in chunks.iter().zip(vecs.into_iter()) {
            points.push(json!({
                "id": chunk_id(&cfg, &rel, c),
                "vector": {
                    "dense": v,
                    "sparse": { "text": docs_point_text(&cfg.repo, &rel, lang, c), "model": "qdrant/bm25" }
                },
                "payload": {
                    "repo": cfg.repo,
                    "path": rel,
                    "language": lang,
                    "symbol": c.label,
                    "kind": c.kind,
                    "line": c.line,
                    "chars": c.body.len(),
                    "indexed_at": now(),
                    "text": c.body
                }
            }));
        }
        for part in points.chunks(64) {
            let body = json!({ "points": part });
            let path =
                format!("/collections/{}/points?wait=false", cfg.collection);
            if let Err(e) = qd(&cfg, "PUT", &path, Some(body)) {
                eprintln!("upsert failed on {}: {}", rel, e);
                return;
            }
        }
        nfiles += 1;
        nchunks += points.len();
        if nfiles == 1 || nfiles % 25 == 0 {
            println!("  {} files, {} chunks, last: {}", nfiles, nchunks, rel);
        }
    }

    println!(
        "done        {} files, {} chunks in {:?}",
        nfiles,
        nchunks,
        t0.elapsed()
    );
    if cfg.dry {
        println!("(dry run - nothing written)");
    }
}

fn parse_url(u: &str) -> (String, Option<String>, Option<String>) {
    let mut url = u.trim().trim_end_matches("/").to_string();
    let mut branch = None;
    let mut sub = None;
    if let Some(idx) = url.find("/tree/") {
        let rest = url[idx + 6..].to_string();
        url = url[..idx].to_string();
        let mut it = rest.splitn(2, "/");
        branch = it.next().map(|s| s.to_string());
        sub = it.next().map(|s| s.to_string());
    }
    if !url.ends_with(".git") {
        url.push_str(".git");
    }
    (url, branch, sub)
}

fn repo_name(url: &str) -> String {
    url.trim_end_matches(".git")
        .rsplit("/")
        .next()
        .unwrap_or("repo")
        .to_string()
}

fn clone_repo(
    cfg: &Cfg,
    url: &str,
    branch: Option<&str>,
) -> Result<PathBuf, String> {
    let work = PathBuf::from(env_or("WORK_DIR", "/workspace/downloads/gh"));
    fs::create_dir_all(&work).map_err(|e| e.to_string())?;
    let dir = work.join(&cfg.repo);
    if dir.join(".git").exists() {
        let d = dir.to_string_lossy().to_string();
        let _ = Command::new("git")
            .args(["-C", &d, "fetch", "--depth", "1", "origin"])
            .output();
        let _ = Command::new("git")
            .args(["-C", &d, "reset", "--hard", "FETCH_HEAD"])
            .output();
        println!("reused checkout");
        return Ok(dir);
    }
    let mut cmd = Command::new("git");
    cmd.arg("clone").arg("--depth").arg("1").arg("--quiet");
    if let Some(b) = branch {
        cmd.arg("--branch").arg(b);
    }
    cmd.arg(url).arg(&dir);
    let out = cmd.output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).to_string());
    }
    Ok(dir)
}

fn lang_for(name: &str) -> Option<&str> {
    match name {
        "Dockerfile" | "Containerfile" => return Some("dockerfile"),
        "Makefile" | "Justfile" => return Some("bash"),
        _ => {}
    }
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())?
        .to_lowercase();
    Some(match ext.as_str() {
        "rs" => "rust",
        "py" => "python",
        "ts" => "typescript",
        "tsx" => "tsx",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "sh" | "bash" => "bash",
        "md" => "markdown",
        "toml" => "toml",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "wit" => "wit",
        "sql" => "sql",
        "css" => "css",
        "html" => "html",
        "ftl" => "fluent",
        _ => return None,
    })
}

fn walk(root: &Path, out: &mut Vec<PathBuf>) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = match fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for ent in rd.flatten() {
            let name = ent.file_name().to_string_lossy().to_string();
            if name.starts_with(".") {
                continue;
            }
            let p = ent.path();
            let md = match ent.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if md.is_dir() {
                if SKIP_DIRS.iter().any(|d| *d == name) {
                    continue;
                }
                stack.push(p);
            } else if md.is_file() {
                if md.len() > MAX_FILE {
                    continue;
                }
                if lang_for(&name).is_none() {
                    continue;
                }
                out.push(p);
            }
        }
    }
    out.sort();
}

fn split_long(text: &str, limit: usize) -> Vec<String> {
    if text.len() <= limit {
        return vec![text.to_string()];
    }
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for ln in text.lines() {
        if cur.len() + ln.len() + 1 > limit && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        cur.push_str(ln);
        cur.push_str("\n");
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn push_block(
    out: &mut Vec<Chunk>,
    kind: &str,
    title: &str,
    lines: &[String],
    line: usize,
) {
    let joined = lines.join("\n");
    let body = joined.trim();
    if body.len() < MIN_CHUNK {
        return;
    }
    let parts = split_long(body, MAX_CHUNK);
    let n = parts.len();
    for (i, p) in parts.into_iter().enumerate() {
        let l = if n > 1 {
            format!("{} [{}/{}]", title, i + 1, n)
        } else {
            title.to_string()
        };
        out.push(Chunk {
            label: l,
            kind: kind.to_string(),
            line,
            body: p,
        });
    }
}

fn chunk_markdown(text: &str) -> Vec<Chunk> {
    let mut out: Vec<Chunk> = Vec::new();
    let mut cur: Vec<String> = Vec::new();
    let mut title = String::from("(intro)");
    let mut start = 1usize;
    let mut ln_no = 1usize;
    for ln in text.lines() {
        if ln.starts_with("# ")
            || ln.starts_with("## ")
            || ln.starts_with("### ")
        {
            push_block(&mut out, "section", &title, &cur, start);
            cur.clear();
            title = ln.trim_start_matches("#").trim().to_string();
            start = ln_no;
        } else {
            cur.push(ln.to_string());
        }
        ln_no += 1;
    }
    push_block(&mut out, "section", &title, &cur, start);
    out
}

fn chunk_toml(text: &str) -> Vec<Chunk> {
    let mut out: Vec<Chunk> = Vec::new();
    let mut cur: Vec<String> = Vec::new();
    let mut title = String::from("(root)");
    let mut start = 1usize;
    let mut ln_no = 1usize;
    for ln in text.lines() {
        let is_head = ln.starts_with("[") && ln.contains("]");
        if is_head {
            push_block(&mut out, "table", &title, &cur, start);
            cur.clear();
            title = ln.trim().to_string();
            start = ln_no;
        } else {
            cur.push(ln.to_string());
        }
        ln_no += 1;
    }
    push_block(&mut out, "table", &title, &cur, start);
    out
}

fn symbol_kinds(lang: &str) -> Vec<&str> {
    match lang {
        "rust" => vec![
            "function_item",
            "struct_item",
            "enum_item",
            "trait_item",
            "const_item",
            "static_item",
            "macro_definition",
            "type_item",
            "union_item",
        ],
        "python" => vec!["function_definition"],
        "typescript" => vec![
            "function_declaration",
            "method_definition",
            "interface_declaration",
            "type_alias_declaration",
            "enum_declaration",
            "arrow_function",
            "lexical_declaration",
        ],
        "tsx" => vec![
            "function_declaration",
            "method_definition",
            "interface_declaration",
            "type_alias_declaration",
            "enum_declaration",
            "arrow_function",
            "lexical_declaration",
        ],
        "javascript" => vec![
            "function_declaration",
            "method_definition",
            "arrow_function",
            "lexical_declaration",
        ],
        _ => vec![],
    }
}

fn name_of(node: tree_sitter::Node, src: &[u8]) -> String {
    if let Some(n) = node.child_by_field_name("name") {
        return String::from_utf8_lossy(&src[n.start_byte()..n.end_byte()])
            .to_string();
    }
    let mut cur = node.walk();
    let mut last = String::new();
    for ch in node.children(&mut cur) {
        let k = ch.kind();
        if k == "identifier"
            || k == "type_identifier"
            || k == "field_identifier"
            || k == "property_identifier"
        {
            last =
                String::from_utf8_lossy(&src[ch.start_byte()..ch.end_byte()])
                    .to_string();
        }
    }
    last
}

fn collect(
    node: tree_sitter::Node,
    src: &[u8],
    kinds: &[&str],
    prefix: &str,
    out: &mut Vec<Chunk>,
    depth: usize,
    skip_tests: bool,
) {
    if depth > 40 {
        return;
    }
    let mut cur = node.walk();
    for child in node.children(&mut cur) {
        let t = child.kind();
        if CONTAINERS.contains(&t) {
            let nm = name_of(child, src);
            let np = if nm.is_empty() {
                prefix.to_string()
            } else {
                format!("{}{}::", prefix, nm)
            };
            collect(child, src, kinds, &np, out, depth + 1, skip_tests);
        } else if kinds.contains(&t) {
            let body = String::from_utf8_lossy(
                &src[child.start_byte()..child.end_byte()],
            )
            .to_string();
            if body.trim().len() < MIN_CHUNK {
                continue;
            }
            let nm = name_of(child, src);
            let base = if nm.is_empty() { t.to_string() } else { nm };
            if skip_tests
                && (base == "tests"
                    || base.ends_with("_tests")
                    || body.trim_start().starts_with("#[cfg(test)]"))
            {
                continue;
            }
            let line = child.start_position().row + 1;
            let parts = split_long(&body, MAX_CHUNK);
            let n = parts.len();
            let label = format!("{}{}", prefix, base);
            for (i, p) in parts.into_iter().enumerate() {
                let l = if n > 1 {
                    format!("{} [{}/{}]", label, i + 1, n)
                } else {
                    label.clone()
                };
                out.push(Chunk {
                    label: l,
                    kind: t.to_string(),
                    line,
                    body: p,
                });
            }
        } else {
            collect(child, src, kinds, prefix, out, depth + 1, skip_tests);
        }
    }
}

fn chunk_ts(lang: &str, text: &str, skip_tests: bool) -> Option<Vec<Chunk>> {
    let language = match lang {
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        "python" => tree_sitter_python::LANGUAGE.into(),
        "javascript" => tree_sitter_javascript::LANGUAGE.into(),
        "typescript" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "tsx" => tree_sitter_typescript::LANGUAGE_TSX.into(),
        _ => return None,
    };
    let kinds = symbol_kinds(lang);
    if kinds.is_empty() {
        return None;
    }
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).ok()?;
    let tree = parser.parse(text, None)?;
    let src = text.as_bytes();
    let mut out: Vec<Chunk> = Vec::new();
    collect(tree.root_node(), src, &kinds, "", &mut out, 0, skip_tests);
    Some(out)
}

fn chunk_file(
    rel: &str,
    lang: &str,
    text: &str,
    skip_tests: bool,
) -> Vec<Chunk> {
    if lang == "markdown" {
        return chunk_markdown(text);
    }
    if lang == "toml" {
        return chunk_toml(text);
    }
    if let Some(v) = chunk_ts(lang, text, skip_tests) {
        if !v.is_empty() {
            return v;
        }
    }
    let base = Path::new(rel)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| rel.to_string());
    let parts = split_long(text, MAX_CHUNK);
    let n = parts.len();
    let mut out = Vec::new();
    for (i, p) in parts.into_iter().enumerate() {
        let l = if n > 1 {
            format!("{} [{}/{}]", base, i + 1, n)
        } else {
            base.clone()
        };
        out.push(Chunk {
            label: l,
            kind: "file".to_string(),
            line: 1,
            body: p,
        });
    }
    out
}

fn doc_text(repo: &str, rel: &str, lang: &str, c: &Chunk) -> String {
    format!(
        "search_document: # repo: {} / {} / {} / {}\n{}",
        repo, rel, lang, c.label, c.body
    )
}

fn docs_point_text(repo: &str, rel: &str, lang: &str, c: &Chunk) -> String {
    format!(
        "# repo: {} / {} / {} / {}\n{}",
        repo, rel, lang, c.label, c.body
    )
}

fn chunk_id(cfg: &Cfg, rel: &str, c: &Chunk) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    c.body.hash(&mut h);
    c.label.hash(&mut h);
    let hh = h.finish();
    let ns = uuid::Uuid::parse_str(NS_UUID).unwrap();
    let key = format!("{}:{}:{}:{}:{:x}", cfg.repo, rel, c.label, c.line, hh);
    uuid::Uuid::new_v5(&ns, key.as_bytes()).to_string()
}

fn qd(
    cfg: &Cfg,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> Result<Value, String> {
    let url = format!("{}{}", cfg.qdrant, path);
    let json_type = "application/json";
    let mut req = match method {
        "GET" => ureq::get(&url),
        "PUT" => ureq::put(&url),
        "POST" => ureq::post(&url),
        _ => return Err("bad method".to_string()),
    };
    req = req.set("Content-Type", json_type);
    req = req.set(cfg.auth_header.as_str(), cfg.apikey.as_str());
    let res = match body {
        Some(b) => req.send_json(b),
        None => req.call(),
    };
    match res {
        Ok(r) => {
            let v: Value = r.into_json().unwrap_or(Value::Null);
            Ok(v)
        }
        Err(ureq::Error::Status(code, r)) => Err(format!(
            "HTTP {} {}",
            code,
            r.into_string().unwrap_or_default()
        )),
        Err(e) => Err(e.to_string()),
    }
}

fn embed(cfg: &Cfg, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
    let url = format!("{}/api/embed", cfg.ollama);
    let body = json!({ "model": cfg.model, "input": texts, "truncate": true });
    let req = ureq::post(&url).set("Content-Type", "application/json");
    let res = req.send_json(body);
    let v: Value = match res {
        Ok(r) => r.into_json().map_err(|e| e.to_string())?,
        Err(ureq::Error::Status(code, r)) => {
            return Err(format!(
                "HTTP {} {}",
                code,
                r.into_string().unwrap_or_default()
            ));
        }
        Err(e) => return Err(e.to_string()),
    };
    let arr = match v.get("embeddings").and_then(|x| x.as_array()) {
        Some(a) => a,
        None => return Err("no embeddings field".to_string()),
    };
    let mut out: Vec<Vec<f32>> = Vec::new();
    for e in arr {
        let items = match e.as_array() {
            Some(a) => a,
            None => return Err("bad vector".to_string()),
        };
        let vv: Vec<f32> = items
            .iter()
            .map(|x| x.as_f64().unwrap_or(0.0) as f32)
            .collect();
        out.push(vv);
    }
    Ok(out)
}

fn ensure_collection(cfg: &Cfg, dim: usize) -> Result<(), String> {
    let path = format!("/collections/{}", cfg.collection);
    if qd(cfg, "GET", &path, None).is_ok() {
        println!("collection  {} already exists", cfg.collection);
        return Ok(());
    }
    let body = json!({
        "vectors": { "dense": { "size": dim, "distance": "Cosine" } },
        "sparse_vectors": { "sparse": { "modifier": "idf" } }
    });
    qd(cfg, "PUT", &path, Some(body))?;
    println!("collection  {} created (dim {})", cfg.collection, dim);
    let idx = format!("/collections/{}/index?wait=true", cfg.collection);
    let text_schema = json!({ "type": "text", "tokenizer": "prefix", "min_token_len": 2, "max_token_len": 30, "lowercase": true });
    let fields: Vec<(String, Value)> = vec![
        ("language".to_string(), json!("keyword")),
        ("kind".to_string(), json!("keyword")),
        ("repo".to_string(), json!("keyword")),
        ("path".to_string(), text_schema),
    ];
    for pair in fields {
        let b = json!({ "field_name": pair.0, "field_schema": pair.1 });
        let _ = qd(cfg, "PUT", &idx, Some(b));
    }
    println!("indexes     language, kind, repo, path");
    Ok(())
}
