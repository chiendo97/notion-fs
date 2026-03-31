# Notion FUSE Filesystem Design

## Purpose

A read-only FUSE filesystem that presents Notion tickets as markdown files organized in a `project/assignee/status/ticket-id.md` hierarchy. Runs inside a Podman container for clean isolation of FUSE dependencies. Primary use cases: browsing/exploring tickets with standard Unix tools (`ls`, `cat`, `grep`) and as a demo of the Notion API.

## Filesystem Structure

```
/mnt/notion/
├── <project>/
│   ├── <assignee>/
│   │   ├── <status>/
│   │   │   ├── GB-319.md
│   │   │   └── GB-412.md
│   │   └── ...
│   └── .refresh
└── <project2>/
    └── ...
```

- Directory names are slugified: lowercased, spaces replaced with hyphens
- Ticket filenames use the human-readable ID (e.g., `GB-319.md`)
- Each project has a `.refresh` sentinel file at its root
- All directories: mode `0o555`. All files: mode `0o444`

### Ticket File Format

```markdown
---
ticket: GB-319
title: Fix login timeout
status: In Progress
priority: High
assignee: Alice
ah: 4.5
created: 2026-03-20
edited: 2026-03-28
url: https://notion.so/...
---

The login flow times out after 30 seconds when...
```

YAML frontmatter with all ticket metadata, followed by the ticket description body.

## Architecture

### Components

1. **`notion_fs.py`** — single-file FUSE filesystem using `fusepy` with PEP 723 inline metadata. Contains:
   - `NotionFS(fuse.Operations)` — FUSE operations class
   - `NotionCache` — fetches tickets from Notion API, builds in-memory directory tree
   - Entrypoint: parse config, populate cache, mount

2. **Notion API access** — direct HTTP calls using `httpx`. Same patterns as the existing `notion_cli.py` (database queries with pagination, property extraction), but implemented independently as a standalone project.

3. **`Containerfile`** — builds a minimal image for Podman with FUSE support.

### Data Flow

```
Mount / .refresh read
        |
        v
  NotionCache.refresh(project?)
        |
        v
  Notion API: query databases per project, paginate
        |
        v
  Build in-memory tree:
    dict[project][assignee][status] -> list[Ticket]
        |
        v
  FUSE serves reads from cache
    readdir() -> list keys at depth
    read()    -> render ticket as frontmatter + body
    getattr() -> dir/file stat based on path depth
```

### Path Resolution

Parse path into segments. Depth determines behavior:

| Depth | Path                              | Type | Content              |
|-------|-----------------------------------|------|----------------------|
| 0     | `/`                               | dir  | list projects        |
| 1     | `/genbook`                        | dir  | list assignees       |
| 1     | `/genbook/.refresh`               | file | trigger refresh      |
| 2     | `/genbook/alice`                  | dir  | list statuses        |
| 3     | `/genbook/alice/in-progress`      | dir  | list tickets         |
| 4     | `/genbook/alice/in-progress/GB-319.md` | file | ticket markdown |

### Future-Proofing for Write Support

The code is structured to make adding write support straightforward later:

- Path resolution is separated from FUSE operation handlers
- `Ticket` model is rich enough to round-trip (parse frontmatter back to fields)
- Write operations (`write`, `truncate`, `open` with write flags) return `EROFS` but are explicitly stubbed, not omitted
- The cache layer has a clear boundary where write-through to the Notion API could be added

## Cache & Refresh

- **Initial load:** `NotionCache.refresh()` fetches all tickets on mount. Filesystem is empty until this completes.
- **In-memory structure:** `tree: dict[str, dict[str, dict[str, list[Ticket]]]]` mapping `project -> assignee -> status -> tickets`
- **Manual refresh:** Reading `.refresh` triggers a re-fetch for that project. Returns `"Refreshed <project>: N tickets\n"`.
- **No background polling.** No TTLs. Data updates only via `.refresh`.
- **Thread safety:** Build a new tree, atomically swap the reference. Old tree stays valid for in-flight reads until GC.
- **Slugification:** Bidirectional mapping between slugs (`in-progress`) and display names (`In Progress`).

## Podman Setup

### Containerfile

- **Base:** `python:3.12-slim`
- **System packages:** `fuse`
- **Python packages:** `fusepy`, `httpx`, `pyyaml`
- **Copy:** `notion_fs.py`
- **Entrypoint:** `python notion_fs.py /mnt/notion`

### Run Command

```bash
podman run --rm -it \
  --device /dev/fuse \
  --cap-add SYS_ADMIN \
  -v ./config:/config:ro \
  -e NOTION_TOKEN=ntn_... \
  -v ./mnt:/mnt/notion:rw,shared \
  notion-fs
```

- `--device /dev/fuse` — FUSE device access
- `--cap-add SYS_ADMIN` — required for `mount()` syscall
- Config mounted read-only at `/config`
- FUSE mountpoint bind-mounted with `:shared` propagation so host sees it at `./mnt/`
- Token passed as env var

### Config

Reuses existing `notion.yaml` format. Required fields:

- `projects`: map of project name to `{ticket_db, epic_db, date_property}` — only `ticket_db` (the Notion database ID) is needed for this filesystem
- `users`: map of display name to Notion user ID — used to resolve assignee names from user IDs in API responses

The `NOTION_TOKEN` env var provides the API integration token.

## Scope

### In Scope

- Read-only FUSE filesystem with `ls`, `cat`, `stat`
- `project/assignee/status/ticket.md` hierarchy
- Manual refresh via `.refresh` sentinel files
- Single `notion_fs.py` file with PEP 723 inline metadata
- Containerfile for Podman with FUSE device passthrough
- Reuse `notion.yaml` config format

### Out of Scope (v1)

- Write support (stubbed with `EROFS`, structured for future addition)
- Alternate views via symlinks
- Background polling or TTL cache
- Ticket creation via filesystem
- Search/filtering beyond directory hierarchy
