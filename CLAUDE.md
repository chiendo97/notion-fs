# notion-fs

FUSE filesystem that presents Notion tickets as markdown files: `project/assignee/status/ticket-id.md`

## Running

```bash
# Local (needs libfuse2)
uv run main.py ./mnt --config /path/to/notion.yaml

# Podman
podman run --rm -it --device /dev/fuse --cap-add SYS_ADMIN \
  -v ./config:/config:ro -e NOTION_TOKEN=ntn_... \
  -v ./mnt:/mnt/notion:rw,shared notion-fs
```

## Lessons Learned

### NixOS + fusepy

- `ctypes.util.find_library("fuse")` returns `None` on NixOS even with nix-ld. Python's ctypes doesn't use the nix-ld interceptor. We monkey-patch `find_library` and glob the nix store for the `.so`.
- fusepy needs **libfuse2**, not libfuse3. Loading libfuse3 makes it mount but all operations return "Operation not supported" (different ABI).

### Notion API Performance

- With 1600+ tickets across multiple databases, initial metadata-only load takes ~45 seconds.
- Fetching page content (descriptions) per ticket on initial load is not viable. Descriptions must be lazy-loaded on `cat`, not on mount.

### fusepy Error Handling

- fusepy expects `raise fuse.FuseOSError(errno.ENOENT)`, not `return -errno.ENOENT`. The return-negative-int pattern is from the C FUSE API, not the Python wrapper. The error message ("'int' object has no attribute 'items'") is misleading.

### FUSE st_size Gotchas

- `ls -la` calls `getattr` on every file. If `getattr` triggers expensive work (like API calls to compute file size), listing a directory with many files hangs.
- The kernel truncates `read()` responses at `st_size`. For dynamic content (like `.refresh` output), use a generous fixed estimate (e.g., 4096) rather than pre-computing the exact size.
- When `read()` returns fewer bytes than `st_size`, the kernel calls `read()` again at the new offset to probe for more data. For virtual files that trigger side effects (like `.refresh` triggering an API call), gate the side effect on `offset == 0` and buffer the result. Otherwise each `cat` triggers the side effect twice.
