# notion-fs Rust Design

## Goal

Implement notion-fs as a Rust FUSE filesystem to improve performance, distribution, maintainability, and provide a focused Rust learning exercise.

## Scope

Read-only Notion ticket filesystem with the existing directory layout, config format, CLI interface, and lazy-loading behavior. Linux only.

## Architecture

Single binary, four modules:

```
src/
  main.rs      CLI args (clap), config loading (serde_yaml), mount entry point
  notion.rs    Notion API client (reqwest::blocking), Ticket struct, property readers
  cache.rs     In-memory tree + JSON disk cache, RwLock for thread safety
  fs.rs        fuser::Filesystem impl (getattr, readdir, read, open, read-only stubs)
```

All synchronous -- no async runtime. `fuser` callbacks call the Notion client directly.

## Data Flow

1. **Startup**: Parse CLI args, load YAML config, attempt JSON disk cache load. If no cache, fetch all projects from Notion API with progress bars (indicatif). Mount filesystem.
2. **readdir / getattr**: Served from in-memory tree. No API calls. Tree type: `HashMap<String, HashMap<String, HashMap<String, Vec<Ticket>>>>` (project -> assignee -> status -> tickets).
3. **read() on ticket.md**: Render frontmatter from cached metadata. If description is empty, fetch page blocks from Notion API synchronously, cache in memory, persist to disk.
4. **read() on .refresh**: Re-fetch all tickets for that project, rebuild tree, clear rendered cache. Show progress on stderr.

## Thread Safety

`fuser` dispatches callbacks from multiple threads. The cache uses `RwLock`:
- `readdir`, `getattr`, `read` (cache hit): read lock (concurrent)
- `read` (description fetch), `.refresh`: write lock (exclusive)

## Modules

### main.rs

- `clap` derive-based CLI: positional `mountpoint`, `--config`, `--cache-dir` (default `./cache`)
- Read `NOTION_TOKEN` from env, exit 1 if missing
- Load config via `serde_yaml`, exit 1 if no projects
- Initialize cache, attempt disk load, else fetch with progress bars
- Mount via `fuser::mount2(fs, mountpoint, &[])`

### notion.rs

- `NOTION_API_URL`, `NOTION_VERSION` constants
- `NotionClient` struct holding token and `reqwest::blocking::Client`
- Methods: `query_database(db_id) -> Vec<Page>`, `get_page_blocks(page_id) -> String`
- Pagination: follow `has_more` / `next_cursor`
- `Ticket` struct with `serde::Serialize + Deserialize`:
  - `ticket_id`, `name`, `status`, `priority`, `assignee`, `ah`, `created`, `edited`, `url`, `page_id`, `description`
- `Ticket::from_page(page: &Value) -> Ticket` — reads properties using helper functions
- Property readers: `read_title`, `read_status`, `read_select`, `read_people`, `read_unique_id`, `read_url`, `read_number`, `read_timestamp`, `read_formula_date`

### cache.rs

- `NotionCache` struct:
  - `config: Config`
  - `tree: RwLock<Tree>` where `Tree = HashMap<String, HashMap<String, HashMap<String, Vec<Ticket>>>>`
  - `slug_map: RwLock<HashMap<String, String>>`
  - `client: NotionClient`
  - `cache_dir: Option<PathBuf>`
- `slugify(name: &str) -> String` — NFKD normalize, strip combining chars, handle Vietnamese d/D, lowercase, replace non-alnum with hyphens
- `load_from_disk() -> usize` — read JSON cache files per project
- `refresh(project: Option<&str>) -> usize` — fetch from API, rebuild tree, save to disk
- `save_project_cache(proj_slug)` — persist single project to JSON
- `get_tree()` / `get_slug_map()` — read-locked accessors returning cloned data

### fs.rs

- `NotionFS` implements `fuser::Filesystem`
- Internal state: `cache: Arc<NotionCache>`, `rendered: Mutex<HashMap<String, Vec<u8>>>`, `refresh_buf: Mutex<HashMap<String, Vec<u8>>>`
- `getattr`: parse path depth 0-4, return dir/file attrs. Files use 4096 as estimated size.
- `readdir`: list projects / assignees / statuses / ticket files based on path depth
- `read`: handle `.refresh` (trigger re-fetch on offset==0, buffer result) and ticket files (render markdown, lazy-fetch description)
- `open`: reject non-read-only flags with EROFS
- Read-only stubs (write, truncate, mkdir, rmdir, unlink, rename, create, chmod, chown): return EROFS

## Ticket Rendering

Markdown format:

```markdown
---
ticket: PROJ-123
title: Ticket name
status: In Progress
priority: High
assignee: Someone
ah: 3.0
created: 2026-01-15
edited: 2026-03-30
url: https://notion.so/...
---

Description content here.
```

## Config Format

Unchanged YAML format, parsed with `serde_yaml`:

```yaml
default_project: "My Project"
projects:
  My Project:
    database_id: "abc123..."
    epics_database_id: ""
    prop_epic: "Epic"
    epic_status_type: "select"
    date_property: "Sort Date"
    date_property_type: "formula"
users:
  "Display Name": "notion-user-id"
```

Unknown fields ignored (serde's default behavior for structs).

## CLI

```
USAGE: notion-fs <MOUNTPOINT> [OPTIONS]

ARGS:
  <MOUNTPOINT>    Directory to mount the filesystem on

OPTIONS:
  --config <PATH>      Path to notion.yaml config
  --cache-dir <DIR>    Directory for JSON cache files [default: ./cache]
```

Env: `NOTION_TOKEN` (required).

## Progress Bars

`indicatif` crate for TUI progress during initial fetch:

- One spinner per project: `Fetching <project>... <n> tickets`
- Summary line after all projects complete: `Loaded <total> tickets`
- Also shown during `.refresh` (prints to stderr while the FUSE read blocks)

## Dependencies

```toml
[dependencies]
fuser = "0.17"
reqwest = { version = "0.12", default-features = false, features = ["blocking", "json", "rustls-tls"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
serde_yaml = "0.9"
clap = { version = "4", features = ["derive"] }
indicatif = "0.17"
unicode-normalization = "0.1"
libc = "0.2"
ctrlc = "3"
```

10 direct dependencies. No async runtime. fuser 0.17 enables concurrent FUSE
dispatch via `n_threads` + `clone_fd`. reqwest uses rustls (no system OpenSSL).
ctrlc enables clean unmount on Ctrl+C via spawn_mount2.

## Error Handling

- **Startup**: config errors, missing token, missing projects -> stderr + exit(1)
- **FUSE callbacks**: API errors during lazy fetch or refresh -> log to stderr, return EIO to kernel. Never crash the filesystem.
- **Disk cache**: corrupted JSON -> log warning, fall back to API fetch

## Build & Run

```bash
cargo build --release
NOTION_TOKEN=ntn_... ./target/release/notion-fs ./mnt --config ./config/notion.yaml
```

Produces a single static binary (with musl: `cargo build --release --target x86_64-unknown-linux-musl`).

## Non-Goals

- macOS support
- Write support (status changes via filesystem)
- Async runtime
- Background auto-refresh
- Epics support
