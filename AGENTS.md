# Repository Guidelines

## Project Structure & Module Organization

`gh2qdrant` is a small Rust binary (edition 2021) that indexes GitHub
repositories into Qdrant as per-symbol chunks.

- `src/main.rs` — all logic: CLI parsing, `parse_url`, `clone_repo`, `walk`,
  `chunk_*` (markdown, toml, tree-sitter), `embed`, `ensure_collection`.
- `Cargo.toml` / `Cargo.lock` — dependencies (serde_json, ureq, uuid,
  tree-sitter + language grammars).
- `target/` — build artifacts; never commit.
- `README.md` — pipeline description and verified results.

There is no `tests/` directory and no wrapper scripts in this checkout
(`scripts/gh2q.sh` and `scripts/cargo.sh` referenced in the README live
outside this directory).

## Build, Test, and Development Commands

```sh
cargo build --release                    # optimized binary (opt-level = 2)
cargo run -- <github-url> --dry          # chunk only, no writes
cargo run -- <github-url> --collection code_mine --limit-files 50
cargo run -- --help                      # prints usage
```

`--dry` is the cheapest local check: it clones, walks, and chunks without
calling Ollama or Qdrant.

## Coding Style & Naming Conventions

- Use `cargo fmt` and `cargo clippy`; indent with 4 spaces.
- Keep a single-file layout unless a module grows large enough to warrant
  splitting; no `mod` declarations currently exist.
- Naming: `snake_case` functions, `CamelCase` structs (`Cfg`, `Chunk`),
  `UPPER_SNAKE_CASE` constants (e.g. `SKIP_DIRS`).
- Keep functions focused on one pipeline stage (`chunk_toml`, `embed`).

## Testing Guidelines

No test suite exists yet. When adding tests, use Rust's built-in
`#[cfg(test)]` modules and run `cargo test`. Focus on pure helpers:
`parse_url`, `repo_name`, `lang_for`, `split_long`, `chunk_markdown`,
`chunk_toml`. Name tests `fn <behavior>_<expectation>()`.

## Commit & Pull Request Guidelines

This directory is not a Git repository (no history to follow). Use
Conventional Commit prefixes (`feat:`, `fix:`, `docs:`), keep commits
small and scoped. Pull requests should describe the change, list the
command used to verify it, and link related issues. Include a dry-run
summary (files, chunks, timing) for pipeline changes.

## Security & Configuration Tips

- Configuration is environment-driven: `QDRANT_URL`, `QDRANT_API_KEY`,
  `QDRANT_AUTH_HEADER`, `OLLAMA_URL`, `EMBED_MODEL`, `WORK_DIR`.
- Never hardcode API keys; `QDRANT_API_KEY` defaults to a placeholder
  value and must be overridden for real writes.
- Keep the network surface intentional: clones hit GitHub, embeddings hit
  Ollama, and writes hit Qdrant.
