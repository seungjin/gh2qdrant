# gh2qdrant

> **AI-generated code notice:** This project was written by a large language
> model (LLM) coding agent, with human review and direction. Treat it as
> such: verify behavior before relying on it in production.

A single-binary Rust tool that turns a GitHub repository into a searchable
Qdrant collection. It clones the repo, splits the source into per-symbol
chunks with tree-sitter, embeds each chunk with `bge-m3` through Ollama,
and upserts hybrid (dense + BM25 sparse) points.

```
GitHub URL -> git clone --depth 1 -> tree-sitter chunks -> bge-m3 -> Qdrant
```

## License

Dual-licensed under **MIT OR Apache-2.0**, at your option — the same
convention Rust projects typically use. See `LICENSE-MIT` and
`LICENSE-APACHE` for the full texts. Contributions are accepted under the
same terms.

## Requirements

- Rust (edition 2021) and Cargo
- `git` on `PATH`
- An [Ollama](https://ollama.com) server with an embedding model (default
  `bge-m3`)
- A Qdrant instance reachable over HTTP

## Build

```sh
cargo build --release
```

## Usage

```sh
cargo run --release -- https://github.com/uuid-rs/uuid --dry
cargo run --release -- https://github.com/uuid-rs/uuid
cargo run --release -- https://github.com/owner/repo/tree/main/crates/foo
cargo run --release -- git@github.com:owner/repo.git \
  --collection code_mine --limit-files 50
```

| Option | Default | Meaning |
| --- | --- | --- |
| `--collection NAME` | `code_<repo>` | target Qdrant collection |
| `--repo NAME` | from URL | payload `repo` label |
| `--batch N` | `8` | chunks per embedding request |
| `--limit-files N` | unlimited | stop after `N` files |
| `--keep-tests` | off | keep `#[cfg(test)]` modules |
| `--dry` | off | chunk only; no embedding or writes |

### Configuration

All configuration is read from the environment.

| Variable | Default |
| --- | --- |
| `QDRANT_URL` | `http://host.containers.internal:9090` |
| `QDRANT_API_KEY` | placeholder — **must be set** for real writes |
| `QDRANT_AUTH_HEADER` | `api-key` |
| `OLLAMA_URL` | `http://host.containers.internal:9020` |
| `EMBED_MODEL` | `bge-m3` |
| `WORK_DIR` | `$TMPDIR/gh2qdrant/gh` (default; override to keep checkouts elsewhere) |

Never hardcode credentials; export them in your shell instead.

## Why index code in a vector database?

LLMs and coding agents can only reason about code they can *retrieve*.
Grepping a repository answers "where does this string appear?" but not
"where is rate-limit backoff implemented?". A vector database answers the
second question: every piece of code becomes a point with an embedding, so
a natural-language query returns the closest *meaning*, not the closest
substring.

| Need | Plain grep / file tree | Vector index |
| --- | --- | --- |
| "find the token bucket refresh logic" | no (unknown wording) | yes (semantic match) |
| exact identifier lookup | fast | supported via sparse/BM25 |
| cross-language, cross-repo search | manual | one hybrid query |
| feed an agent context | whole files, token heavy | top-k relevant symbols |
| survives renames/refactors | n/a | matched by meaning |

Practical benefits:

- **Cheap context.** Retrieve ~10 relevant symbols instead of pasting a
  400 KB file into a prompt.
- **Hybrid recall.** Dense vectors catch paraphrases; BM25 sparse vectors
  catch exact identifiers such as `parse_nil_hyphenated`.
- **Stable citations.** Every point carries `repo`, `path`, `symbol`, and
  `line`, so an answer can point at real code.
- **Repeatable corpora.** Stable IDs make re-ingestion idempotent, so
  refreshing a collection never duplicates points.

## How to load code: from repo to points

```
GitHub URL --parse--> --clone--> --walk--> --chunk--> --embed--> --upsert--> Qdrant
```

1. **Clone shallowly.** `git clone --depth 1` keeps ingestion fast; only
   the working tree is indexed, not history.
2. **Walk and filter.** Skip dot-directories, build/vendor trees, files
   over 400 KB, and unknown extensions (extension-to-language mapping lives
   in `lang_for`).
3. **Chunk** (next section) so each point is a single coherent unit.
4. **Embed** each chunk text with `bge-m3` (prefix `search_document:`) and
   also store the raw text for BM25 sparse matching.
5. **Upsert** with deterministic IDs:
   `uuid5(repo:path:label:line:bodyhash)`.

### Why chunking matters

You cannot embed a whole repository, and a whole file is a poor retrieval
unit: one embedding averaged over 2,000 lines cannot be "close" to a
narrow question. Chunk boundaries decide retrieval quality.

- **Too large** → the vector is an average of unrelated code; irrelevant
  chunks outrank the right one and cost more tokens.
- **Too small** → symbols lose their signatures, types, and doc comments,
  so the chunk is not self-explanatory.
- **Semantic units win** → a chunk should be exactly one thing a developer
  would name: a function, a struct, a section, a table.

This project also enforces two size bounds: chunks shorter than **80
characters** are dropped as noise, and chunks over **3,600 characters**
are split on line boundaries and labelled `[i/n]`.

### Chunking source code with tree-sitter

Regexes and brace counting break on strings, macros, generic types, and
nested closures. [tree-sitter](https://tree-sitter.github.io/) produces a
real syntax tree, so chunk boundaries land on *symbol boundaries*.

The algorithm in `chunk_ts` / `collect`:

1. Parse the file with the grammar crate for its language (`tree-sitter-rust`,
   `tree-sitter-python`, `tree-sitter-javascript`, `tree-sitter-typescript`).
2. Start at the root node and walk children.
3. If a node is a **container** — `impl_item`, `mod_item`,
   `class_declaration`, `interface_declaration`, `source_file`, and
   similar — recurse into it and extend the name prefix, producing
   qualified labels such as `Uuid::parse_str`.
4. If a node is a **symbol** listed in `symbol_kinds`, emit its exact
   source text as one chunk. Examples: `function_item`, `struct_item`,
   `trait_item` (Rust), `function_definition` (Python),
   `method_definition`, `interface_declaration` (TypeScript).
5. Skip `#[cfg(test)]` modules and `tests` / `*_tests` symbols unless
   `--keep-tests` is passed.
6. Recurse into all other nodes so nested items are still found; depth is
   capped at 40 to bound pathological trees.

Sketch:

```rust
let mut parser = tree_sitter::Parser::new();
parser.set_language(&tree_sitter_rust::LANGUAGE.into())?;
let tree = parser.parse(text, None)?;
let mut out = Vec::new();
collect(tree.root_node(), text.as_bytes(), &symbol_kinds("rust"), "", &mut out, 0, true);
```

Fallbacks when a grammar is not linked or a file yields no symbols:

- **Markdown** → split on `#`, `##`, `###` headings (`kind=section`).
- **TOML** → split on `[table]` headers (`kind=table`).
- **Everything else** → 3,600-character line windows (`kind=file`).

### Querying the collection

Dense and sparse vectors are stored side by side, so one hybrid query can
combine both:

```http
POST /collections/code_uuid/points/query
{
  "prefetch": [
    { "query": { "text": "hyphenated UUID parsing benchmark", "model": "bge-m3" }, "using": "dense", "limit": 20 },
    { "query": { "text": "parse_nil_hyphenated", "model": "qdrant/bm25" }, "using": "sparse", "limit": 20 }
  ],
  "query": { "fusion": "rrf" },
  "limit": 10
}
```

Filter on the indexed payload fields to narrow scope before searching:

```json
{ "must": [ { "key": "repo", "match": { "value": "uuid" } } ] }
```

## Pipeline

1. **Parse URL** — understands `/tree/<branch>/<subpath>` and appends `.git`.
2. **Clone** — `--depth 1` into `WORK_DIR/<repo>`; an existing checkout is
   reused via `fetch` + `reset`.
3. **Walk** — skips dot-directories, build/vendor trees (`target`,
   `node_modules`, `vendor`, `dist`, ...), and files over 400 KB.
4. **Chunk**
   - Markdown → `#`/`##`/`###` headings (`kind=section`)
   - TOML → `[table]` headers (`kind=table`)
   - Rust, Python, TypeScript/TSX, JavaScript → tree-sitter symbols with
     qualified `Type::method` labels; blocks over 3600 chars are split as
     `[i/n]`
   - Everything else → 3600-char line windows (`kind=file`)
5. **Filter** — drops `#[cfg(test)]` modules unless `--keep-tests`.
6. **Embed** — Ollama `/api/embed`, `--batch` inputs per request.
7. **Create collection** — dense 1024-dim Cosine + sparse IDF, with
   keyword indexes on `language`, `kind`, `repo` and a prefix-text index
   on `path`.
8. **Upsert** — deterministic `uuid5(repo:path:label:line:bodyhash)` IDs,
   64 points per request.

Each point carries `repo`, `path`, `language`, `symbol`, `kind`, `line`,
`chars`, `indexed_at`, and `text` in its payload.

## Verified

Measured 2026-10-28:

- `uuid-rs/uuid`: 58 files → 552 chunks in 0.86 s (dry run)
- 10 files → 62 points written; collection green, dimension 1024
- A hybrid query returned `bench::parse_nil_hyphenated` at rank 1

## Known limitations

- Bash, Dockerfile, JSON, YAML, WIT, CSS, and HTML fall back to size-based
  windows (no grammar crate linked).
- Re-running re-embeds everything; IDs are stable, so no duplicates occur.
- Renamed or deleted symbols leave stale points — drop and re-ingest for a
  clean build.
- Clones are shallow, so commit history is unavailable.

## Contributing

See `AGENTS.md` for build, style, and commit conventions.

## Disclaimer

This software is provided "as is", without warranty of any kind. Because
the code was LLM-generated, bugs, omissions, or incorrect assumptions may
remain; review it before deployment.
