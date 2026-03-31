# /// script
# requires-python = ">=3.12"
# dependencies = ["fusepy", "pyyaml", "certifi", "pydantic"]
# ///
"""Notion FUSE filesystem — read-only mount of Notion tickets as markdown files."""

from __future__ import annotations

import argparse
import errno
import json
import os
import re
import ssl
import stat
import sys
import threading
import time
import unicodedata
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any


import ctypes.util

# fusepy uses ctypes.util.find_library("fuse") which fails with uv-managed
# Python on NixOS. Patch to check nix-ld and nix store paths.
_orig_find_library = ctypes.util.find_library


def _patched_find_library(name: str) -> str | None:
    result = _orig_find_library(name)
    if result is not None:
        return result
    if name == "fuse":
        import glob as globmod
        candidates = ["/run/current-system/sw/share/nix-ld/lib/libfuse.so"]
        candidates.extend(globmod.glob("/nix/store/*/lib/libfuse.so"))
        for path in candidates:
            if os.path.exists(path):
                return path
    return None


ctypes.util.find_library = _patched_find_library

import certifi  # noqa: E402
import fuse  # noqa: E402
import yaml  # noqa: E402
from pydantic import BaseModel, ConfigDict

# =============================================================================
# Constants
# =============================================================================

NOTION_API_URL = "https://api.notion.com/v1"
NOTION_VERSION = "2022-06-28"
SSL_CTX = ssl.create_default_context(cafile=certifi.where())

DEFAULT_CONFIG_PATHS = [
    Path("/config/notion.yaml"),
    Path("./config/notion.yaml"),
]


# =============================================================================
# Pydantic Models
# =============================================================================


class ProjectConfig(BaseModel):
    model_config = ConfigDict(extra="ignore")  # pyright: ignore[reportUnannotatedClassAttribute]

    database_id: str
    epics_database_id: str = ""
    prop_epic: str = "Epic"
    epic_status_type: str = "select"
    date_property: str = "Sort Date"
    date_property_type: str = "formula"


class Config(BaseModel):
    model_config = ConfigDict(extra="ignore")  # pyright: ignore[reportUnannotatedClassAttribute]

    default_project: str = ""
    projects: dict[str, ProjectConfig] = {}
    users: dict[str, str] = {}


# =============================================================================
# HTTP helpers
# =============================================================================

_token: str = ""


def _headers() -> dict[str, str]:
    return {
        "Authorization": f"Bearer {_token}",
        "Notion-Version": NOTION_VERSION,
        "Content-Type": "application/json",
    }


def _request(method: str, path: str, body: dict[str, Any] | None = None) -> dict[str, Any]:
    url = f"{NOTION_API_URL}{path}"
    data = json.dumps(body).encode() if body else None
    req = urllib.request.Request(url, data=data, headers=_headers(), method=method)
    try:
        with urllib.request.urlopen(req, context=SSL_CTX) as resp:
            return json.loads(resp.read())  # pyright: ignore[reportAny]
    except urllib.error.HTTPError as e:
        error_body = e.read().decode()
        print(f"API Error ({e.code}): {error_body}", file=sys.stderr)
        raise
    except urllib.error.URLError as e:
        print(f"Network Error: {e.reason}", file=sys.stderr)
        raise


def _post(path: str, body: dict[str, Any]) -> dict[str, Any]:
    return _request("POST", path, body)


def _get(path: str) -> dict[str, Any]:
    return _request("GET", path)


# =============================================================================
# Property readers
# =============================================================================


def _read_title(props: dict[str, Any], key: str = "Name") -> str:
    title = props.get(key, {}).get("title", [])
    return title[0]["plain_text"] if title else ""  # pyright: ignore[reportAny]


def _read_status(props: dict[str, Any]) -> str:
    raw = props.get("Status", {})
    prop_type = raw.get("type", "")
    val = raw.get(prop_type, {})
    return val.get("name", "") if val else ""  # pyright: ignore[reportAny]


def _read_select(props: dict[str, Any], key: str) -> str:
    sel = props.get(key, {}).get("select", {})
    return sel.get("name", "") if sel else ""  # pyright: ignore[reportAny]


def _read_people(props: dict[str, Any], key: str = "Assignee") -> str:
    people = props.get(key, {}).get("people", [])
    return people[0].get("name", "") if people else ""  # pyright: ignore[reportAny]


def _read_unique_id(props: dict[str, Any]) -> str:
    for raw_prop in props.values():
        prop: dict[str, Any] = raw_prop if isinstance(raw_prop, dict) else {}  # pyright: ignore[reportAny]
        if prop.get("type") == "unique_id":
            uid: dict[str, Any] = prop.get("unique_id") or {}
            prefix = str(uid.get("prefix", ""))
            number_val = uid.get("number")
            if prefix and number_val is not None:
                return f"{prefix}-{number_val}"
    return ""


def _read_url(props: dict[str, Any], key: str) -> str:
    return props.get(key, {}).get("url", "") or ""  # pyright: ignore[reportAny]


def _read_number(props: dict[str, Any], key: str) -> float | None:
    val = props.get(key, {}).get("number")
    if val is None:
        return None
    return float(val)  # pyright: ignore[reportAny]


def _read_timestamp(props: dict[str, Any], key: str) -> str:
    raw = props.get(key, {})
    prop_type = raw.get("type", "")
    return raw.get(prop_type, "") or ""  # pyright: ignore[reportAny]


def _read_formula_date(props: dict[str, Any], key: str) -> str:
    raw = props.get(key, {}).get("formula", {})
    if raw.get("type") == "date" and raw.get("date"):
        return raw["date"].get("start", "") or ""  # pyright: ignore[reportAny]
    return ""


# =============================================================================
# Block reader
# =============================================================================


def _read_page_content(page_id: str) -> str:
    parts: list[str] = []
    url = f"/blocks/{page_id}/children"
    while True:
        resp = _get(url)
        for block in resp.get("results", []):  # pyright: ignore[reportAny]
            btype = block.get("type", "")
            texts = block.get(btype, {}).get("rich_text", [])
            text = "".join(t.get("plain_text", "") for t in texts)  # pyright: ignore[reportAny, reportUnknownVariableType]
            if text:
                parts.append(text)  # pyright: ignore[reportUnknownArgumentType]
        if not resp.get("has_more"):
            break
        url = f"/blocks/{page_id}/children?start_cursor={resp['next_cursor']}"
    return "\n\n".join(parts)


# =============================================================================
# Query helper
# =============================================================================


def _query_database(database_id: str, body: dict[str, Any] | None = None) -> list[dict[str, Any]]:
    payload = dict(body) if body else {}
    all_results: list[dict[str, Any]] = []
    has_more = True
    while has_more:
        resp = _post(f"/databases/{database_id}/query", payload)
        all_results.extend(resp.get("results", []))  # pyright: ignore[reportAny]
        has_more = resp.get("has_more", False)  # pyright: ignore[reportAny]
        next_cursor = resp.get("next_cursor")
        if has_more and next_cursor:
            payload["start_cursor"] = next_cursor
        else:
            has_more = False
    return all_results


# =============================================================================
# Ticket model
# =============================================================================


class Ticket(BaseModel):
    ticket_id: str = ""
    name: str = ""
    status: str = ""
    priority: str = ""
    assignee: str = ""
    ah: float | None = None
    created: str = ""
    edited: str = ""
    url: str = ""
    page_id: str = ""
    description: str = ""

    @classmethod
    def from_page(cls, page: dict[str, Any]) -> Ticket:
        props = page.get("properties", {})
        return cls(
            ticket_id=_read_unique_id(props),
            name=_read_title(props),
            status=_read_status(props),
            priority=_read_select(props, "Priority"),
            assignee=_read_people(props),
            ah=_read_number(props, "AH"),
            created=_read_timestamp(props, "Created time"),
            edited=_read_timestamp(props, "Last edited time"),
            url=page.get("url", ""),
            page_id=page.get("id", ""),
        )

    def fetch_description(self) -> None:
        if self.page_id:
            self.description = _read_page_content(self.page_id)

    def render(self) -> tuple[bytes, bool]:
        """Render ticket as markdown. Returns (content, description_was_fetched)."""
        fetched = False
        if not self.description and self.page_id:
            self.fetch_description()
            fetched = True
        ah_str = str(self.ah) if self.ah is not None else ""
        created_short = self.created[:10] if self.created else ""
        edited_short = self.edited[:10] if self.edited else ""
        lines = [
            "---",
            f"ticket: {self.ticket_id}",
            f"title: {self.name}",
            f"status: {self.status}",
            f"priority: {self.priority}",
            f"assignee: {self.assignee}",
            f"ah: {ah_str}",
            f"created: {created_short}",
            f"edited: {edited_short}",
            f"url: {self.url}",
            "---",
            "",
            self.description,
            "",
        ]
        return "\n".join(lines).encode("utf-8"), fetched


# =============================================================================
# Slugify
# =============================================================================


def slugify(name: str) -> str:
    # NFD decompose then strip combining marks (accents/tones)
    nfkd = unicodedata.normalize("NFKD", name)
    ascii_name = "".join(c for c in nfkd if not unicodedata.combining(c))
    # Handle Vietnamese đ/Đ which NFD doesn't decompose
    ascii_name = ascii_name.replace("đ", "d").replace("Đ", "D")
    return re.sub(r"[^a-z0-9]+", "-", ascii_name.lower()).strip("-")


# =============================================================================
# NotionCache
# =============================================================================

Tree = dict[str, dict[str, dict[str, list[Ticket]]]]


class NotionCache:
    def __init__(self, config: Config, cache_dir: Path | None = None) -> None:
        self.config = config
        self.tree: Tree = {}
        self.slug_map: dict[str, str] = {}
        self._lock = threading.Lock()
        self._cache_dir = cache_dir
        self._user_id_to_name: dict[str, str] = {
            uid: name for name, uid in config.users.items()
        }
        if cache_dir:
            cache_dir.mkdir(parents=True, exist_ok=True)

    def _resolve_assignee(self, ticket: Ticket) -> str:
        return ticket.assignee if ticket.assignee else "unassigned"

    def _cache_path(self, proj_slug: str) -> Path | None:
        if not self._cache_dir:
            return None
        return self._cache_dir / f"{proj_slug}.json"

    def _save_cache(self, proj_slug: str, tickets: list[Ticket]) -> None:
        path = self._cache_path(proj_slug)
        if not path:
            return
        data = [t.model_dump() for t in tickets]
        path.write_text(json.dumps(data), encoding="utf-8")

    def _load_cache(self, proj_slug: str) -> list[Ticket] | None:
        path = self._cache_path(proj_slug)
        if not path or not path.exists():
            return None
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
            return [Ticket.model_validate(d) for d in data]
        except (json.JSONDecodeError, ValueError):
            return None

    def load_from_disk(self) -> int:
        total = 0
        new_tree: Tree = {}
        new_slug_map: dict[str, str] = {}
        for proj_name in self.config.projects:
            proj_slug = slugify(proj_name)
            tickets = self._load_cache(proj_slug)
            if tickets is None:
                continue
            new_slug_map[proj_slug] = proj_name
            assignee_dict: dict[str, dict[str, list[Ticket]]] = {}
            for ticket in tickets:
                total += 1
                assignee_display = self._resolve_assignee(ticket)
                assignee_slug = slugify(assignee_display)
                new_slug_map[assignee_slug] = assignee_display
                status_display = ticket.status or "no-status"
                status_slug = slugify(status_display)
                new_slug_map[status_slug] = status_display
                if assignee_slug not in assignee_dict:
                    assignee_dict[assignee_slug] = {}
                if status_slug not in assignee_dict[assignee_slug]:
                    assignee_dict[assignee_slug][status_slug] = []
                assignee_dict[assignee_slug][status_slug].append(ticket)
            new_tree[proj_slug] = assignee_dict
        with self._lock:
            self.tree.update(new_tree)
            self.slug_map.update(new_slug_map)
        return total

    def refresh(self, project: str | None = None) -> int:
        projects = [project] if project else list(self.config.projects.keys())
        new_tree: Tree = {}
        new_slug_map: dict[str, str] = dict(self.slug_map)
        total = 0

        for proj_name in projects:
            proj_config = self.config.projects.get(proj_name)
            if not proj_config:
                continue

            pages = _query_database(proj_config.database_id)
            proj_slug = slugify(proj_name)
            new_slug_map[proj_slug] = proj_name
            assignee_dict: dict[str, dict[str, list[Ticket]]] = {}
            all_tickets: list[Ticket] = []

            for page in pages:
                ticket = Ticket.from_page(page)
                total += 1
                all_tickets.append(ticket)

                assignee_display = self._resolve_assignee(ticket)
                assignee_slug = slugify(assignee_display)
                new_slug_map[assignee_slug] = assignee_display

                status_display = ticket.status or "no-status"
                status_slug = slugify(status_display)
                new_slug_map[status_slug] = status_display

                if assignee_slug not in assignee_dict:
                    assignee_dict[assignee_slug] = {}
                if status_slug not in assignee_dict[assignee_slug]:
                    assignee_dict[assignee_slug][status_slug] = []
                assignee_dict[assignee_slug][status_slug].append(ticket)

            new_tree[proj_slug] = assignee_dict
            self._save_cache(proj_slug, all_tickets)

        with self._lock:
            for proj_slug, assignee_dict in new_tree.items():
                self.tree[proj_slug] = assignee_dict
            self.slug_map.update(new_slug_map)

        return total

    def save_project_cache(self, proj_slug: str) -> None:
        with self._lock:
            assignee_dict = self.tree.get(proj_slug, {})
            all_tickets = [t for statuses in assignee_dict.values() for tickets in statuses.values() for t in tickets]
        self._save_cache(proj_slug, all_tickets)

    def get_tree(self) -> Tree:
        with self._lock:
            return dict(self.tree)

    def get_slug_map(self) -> dict[str, str]:
        with self._lock:
            return dict(self.slug_map)


# =============================================================================
# FUSE Filesystem
# =============================================================================


def _dir_stat(now: float) -> dict[str, int | float]:
    return {
        "st_mode": stat.S_IFDIR | 0o555,
        "st_nlink": 2,
        "st_uid": os.getuid(),
        "st_gid": os.getgid(),
        "st_size": 0,
        "st_atime": now,
        "st_mtime": now,
        "st_ctime": now,
    }


def _file_stat(size: int, now: float) -> dict[str, int | float]:
    return {
        "st_mode": stat.S_IFREG | 0o444,
        "st_nlink": 1,
        "st_uid": os.getuid(),
        "st_gid": os.getgid(),
        "st_size": size,
        "st_atime": now,
        "st_mtime": now,
        "st_ctime": now,
    }


class NotionFS(fuse.Operations):  # pyright: ignore[reportMissingTypeStubs]
    def __init__(self, cache: NotionCache) -> None:
        self.cache = cache
        self._rendered: dict[str, bytes] = {}
        self._refresh_buf: dict[str, bytes] = {}

    def _parse_path(self, path: str) -> list[str]:
        parts = [p for p in path.strip("/").split("/") if p]
        return parts

    def _find_ticket(self, proj: str, assignee: str, status_slug: str, filename: str) -> Ticket | None:
        tree = self.cache.get_tree()
        tickets = tree.get(proj, {}).get(assignee, {}).get(status_slug, [])
        for t in tickets:
            if f"{t.ticket_id}.md" == filename:
                return t
        return None

    def _render_ticket(self, ticket: Ticket, proj_slug: str | None = None) -> bytes:
        key = ticket.page_id
        if key not in self._rendered:
            content, fetched = ticket.render()
            self._rendered[key] = content
            if fetched and proj_slug:
                self.cache.save_project_cache(proj_slug)
        return self._rendered[key]

    def getattr(self, path: str, fh: int | None = None) -> dict[str, int | float]:
        now = time.time()
        parts = self._parse_path(path)

        if len(parts) == 0:
            return _dir_stat(now)

        tree = self.cache.get_tree()

        if len(parts) == 1:
            if parts[0] in tree:
                return _dir_stat(now)
            raise fuse.FuseOSError(errno.ENOENT)

        proj = parts[0]
        if proj not in tree:
            raise fuse.FuseOSError(errno.ENOENT)

        if len(parts) == 2:
            if parts[1] == ".refresh":
                return _file_stat(4096, now)
            if parts[1] in tree[proj]:
                return _dir_stat(now)
            raise fuse.FuseOSError(errno.ENOENT)

        if len(parts) == 3:
            assignee = parts[1]
            status_slug = parts[2]
            if assignee in tree.get(proj, {}) and status_slug in tree.get(proj, {}).get(assignee, {}):
                return _dir_stat(now)
            raise fuse.FuseOSError(errno.ENOENT)

        if len(parts) == 4:
            ticket = self._find_ticket(parts[0], parts[1], parts[2], parts[3])
            if ticket:
                # Use a large estimate for st_size to avoid fetching descriptions
                # during ls -la. The actual content is returned by read().
                return _file_stat(4096, now)
            raise fuse.FuseOSError(errno.ENOENT)

        raise fuse.FuseOSError(errno.ENOENT)

    def readdir(self, path: str, fh: int | None = None) -> list[str]:
        parts = self._parse_path(path)
        entries = [".", ".."]
        tree = self.cache.get_tree()

        if len(parts) == 0:
            entries.extend(tree.keys())
        elif len(parts) == 1:
            proj = parts[0]
            if proj in tree:
                entries.append(".refresh")
                entries.extend(tree[proj].keys())
        elif len(parts) == 2:
            proj, assignee = parts[0], parts[1]
            statuses = tree.get(proj, {}).get(assignee, {})
            entries.extend(statuses.keys())
        elif len(parts) == 3:
            proj, assignee, status_slug = parts[0], parts[1], parts[2]
            tickets = tree.get(proj, {}).get(assignee, {}).get(status_slug, [])
            for t in tickets:
                entries.append(f"{t.ticket_id}.md")

        return entries

    def read(self, path: str, size: int, offset: int, fh: int | None = None) -> bytes:
        parts = self._parse_path(path)

        if len(parts) == 2 and parts[1] == ".refresh":
            proj = parts[0]
            # Only trigger refresh on first read; subsequent reads
            # (from kernel probing past st_size) serve from buffer.
            if offset == 0:
                slug_map = self.cache.get_slug_map()
                proj_display = slug_map.get(proj, proj)
                count = self.cache.refresh(proj_display)
                self._rendered.clear()
                msg = f"Refreshed {proj_display}: {count} tickets\n"
                self._refresh_buf[proj] = msg.encode("utf-8")
            data = self._refresh_buf.get(proj, b"")
            return data[offset : offset + size]

        if len(parts) == 4:
            ticket = self._find_ticket(parts[0], parts[1], parts[2], parts[3])
            if ticket:
                content = self._render_ticket(ticket, proj_slug=parts[0])
                return content[offset : offset + size]

        raise fuse.FuseOSError(errno.ENOENT)

    def open(self, path: str, flags: int) -> int:
        accmode = flags & (os.O_RDONLY | os.O_WRONLY | os.O_RDWR)
        if accmode != os.O_RDONLY:
            raise fuse.FuseOSError(errno.EROFS)
        return 0

    def write(self, path: str, data: bytes, offset: int, fh: int) -> int:
        raise fuse.FuseOSError(errno.EROFS)

    def truncate(self, path: str, length: int, fh: int | None = None) -> int:
        raise fuse.FuseOSError(errno.EROFS)

    def mkdir(self, path: str, mode: int) -> int:
        raise fuse.FuseOSError(errno.EROFS)

    def rmdir(self, path: str) -> int:
        raise fuse.FuseOSError(errno.EROFS)

    def unlink(self, path: str) -> int:
        raise fuse.FuseOSError(errno.EROFS)

    def rename(self, old: str, new: str) -> int:
        raise fuse.FuseOSError(errno.EROFS)

    def create(self, path: str, mode: int, fi: int | None = None) -> int:
        raise fuse.FuseOSError(errno.EROFS)

    def chmod(self, path: str, mode: int) -> int:
        raise fuse.FuseOSError(errno.EROFS)

    def chown(self, path: str, uid: int, gid: int) -> int:
        raise fuse.FuseOSError(errno.EROFS)


# =============================================================================
# Config loading
# =============================================================================


def load_config(config_path: str | None = None) -> Config:
    if config_path:
        p = Path(config_path)
        if not p.exists():
            print(f"Error: config not found at {p}", file=sys.stderr)
            sys.exit(1)
        with open(p) as f:
            raw = dict(yaml.safe_load(f) or {})
            return Config.model_validate(raw)

    for p in DEFAULT_CONFIG_PATHS:
        if p.exists():
            with open(p) as f:
                raw = dict(yaml.safe_load(f) or {})
                return Config.model_validate(raw)

    print("Error: no config file found", file=sys.stderr)
    sys.exit(1)


# =============================================================================
# Entrypoint
# =============================================================================


def main() -> None:
    global _token

    parser = argparse.ArgumentParser(description="Notion FUSE filesystem")
    parser.add_argument("mountpoint", help="Directory to mount the filesystem on")
    parser.add_argument("--config", dest="config_path", default=None, help="Path to notion.yaml config")
    parser.add_argument("--cache-dir", dest="cache_dir", default="./cache", help="Directory for JSON cache files (default: ./cache)")
    args = parser.parse_args()

    _token = os.environ.get("NOTION_TOKEN", "")
    if not _token:
        print("Error: NOTION_TOKEN environment variable is required", file=sys.stderr)
        sys.exit(1)

    config = load_config(args.config_path)
    if not config.projects:
        print("Error: no projects configured", file=sys.stderr)
        sys.exit(1)

    cache_dir = Path(args.cache_dir)
    cache = NotionCache(config, cache_dir=cache_dir)

    total = cache.load_from_disk()
    if total > 0:
        print(f"Loaded {total} tickets from disk cache", file=sys.stderr)
    else:
        print("No disk cache found, fetching from Notion...", file=sys.stderr)
        total = cache.refresh()
        print(f"Loaded {total} tickets", file=sys.stderr)

    print(f"Mounted at {args.mountpoint}", file=sys.stderr)
    fuse.FUSE(  # pyright: ignore[reportMissingTypeStubs]
        NotionFS(cache),
        args.mountpoint,
        foreground=True,
        allow_other=False,
    )


if __name__ == "__main__":
    main()
