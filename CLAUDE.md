# notion-fs

FUSE filesystem that presents Notion tickets as markdown files: `project/assignee/status/ticket-id.md`

## Running

```bash
# Local (needs FUSE and NOTION_TOKEN)
NOTION_TOKEN=ntn_... cargo run -- ./mnt --config ./config/notion.yaml

# Release binary
cargo build --release
NOTION_TOKEN=ntn_... ./target/release/notion-fs ./mnt --config ./config/notion.yaml

# Podman
podman run --rm -it --device /dev/fuse --cap-add SYS_ADMIN \
  -v ./config:/config:ro -e NOTION_TOKEN=ntn_... \
  -v ./mnt:/mnt/notion:rw,shared notion-fs
```

## Lessons Learned

### FUSE Runtime

- `fuser` uses the system FUSE runtime. The container image installs `fuse3`.
- Container runs still need `--device /dev/fuse`, `--cap-add SYS_ADMIN`, and a shared bind mount for the host to see the mounted tree.

### Notion API Performance

- With 1600+ tickets across multiple databases, initial metadata-only load takes ~45 seconds.
- Fetching page content (descriptions) per ticket on initial load is not viable. Descriptions must be lazy-loaded on `cat`, not on mount.

### FUSE Error Handling

- Return `libc` errno values from FUSE callbacks and keep Notion API failures contained to `EIO` for the affected read.
- Keep write-like operations explicitly read-only with `EROFS`.

### FUSE st_size Gotchas

- `ls -la` calls `getattr` on every file. If `getattr` triggers expensive work (like API calls to compute file size), listing a directory with many files hangs.
- The kernel truncates `read()` responses at `st_size`. For dynamic content (like `.refresh` output), use a generous fixed estimate (e.g., 4096) rather than pre-computing the exact size.
- When `read()` returns fewer bytes than `st_size`, the kernel calls `read()` again at the new offset to probe for more data. For virtual files that trigger side effects (like `.refresh` triggering an API call), gate the side effect on `offset == 0` and buffer the result. Otherwise each `cat` triggers the side effect twice.
