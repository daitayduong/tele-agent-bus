#!/usr/bin/env python3
"""
Interactive repair tool for local Codex JSONL sessions.

It lists sessions from CODEX_HOME (default: ~/.codex), lets you choose one,
then shrinks only oversized JSONL records/strings after creating a backup.
This is intentionally standalone and does not depend on agent-bus.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any


DEFAULT_MAX_LINE_BYTES = 256 * 1024
DEFAULT_MAX_STRING_BYTES = 64 * 1024
REPAIR_MARKER = "[codex-session-repair]"


@dataclass
class Session:
    session_id: str
    path: Path
    title: str | None
    cwd: str | None
    updated: float
    bytes_size: int
    lines: int = 0
    max_line_bytes: int = 0
    oversized_lines: int = 0


def codex_home_from_args(value: str | None) -> Path:
    if value:
        return Path(value).expanduser()
    if os.environ.get("CODEX_HOME"):
        return Path(os.environ["CODEX_HOME"]).expanduser()
    return Path.home() / ".codex"


def load_titles(codex_home: Path) -> dict[str, str]:
    titles: dict[str, str] = {}
    index = codex_home / "session_index.jsonl"
    if not index.exists():
        return titles
    with index.open("r", encoding="utf-8", errors="replace") as fh:
        for line in fh:
            try:
                value = json.loads(line)
            except json.JSONDecodeError:
                continue
            session_id = value.get("id")
            title = value.get("thread_name")
            if isinstance(session_id, str) and isinstance(title, str) and title.strip():
                titles[session_id] = one_line(title.strip(), 80)
    return titles


def discover_sessions(codex_home: Path, repo: str | None = None) -> list[Session]:
    titles = load_titles(codex_home)
    root = codex_home / "sessions"
    sessions: list[Session] = []
    if not root.exists():
        return sessions

    repo_norm = normalize_path(repo) if repo else None
    for path in root.rglob("rollout-*.jsonl"):
        meta = read_session_meta(path)
        if not meta:
            continue
        session_id, cwd = meta
        if repo_norm and normalize_path(cwd) != repo_norm:
            continue
        try:
            stat = path.stat()
        except OSError:
            continue
        sessions.append(
            Session(
                session_id=session_id,
                path=path,
                title=titles.get(session_id),
                cwd=cwd,
                updated=stat.st_mtime,
                bytes_size=stat.st_size,
            )
        )

    sessions.sort(key=lambda s: (s.updated, s.session_id), reverse=True)
    return sessions


def read_session_meta(path: Path) -> tuple[str, str | None] | None:
    try:
        with path.open("r", encoding="utf-8", errors="replace") as fh:
            for idx, line in enumerate(fh):
                if idx >= 40:
                    break
                try:
                    value = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if value.get("type") != "session_meta":
                    continue
                payload = value.get("payload") or {}
                session_id = payload.get("id")
                cwd = payload.get("cwd")
                if isinstance(session_id, str):
                    return session_id, cwd if isinstance(cwd, str) else None
    except OSError:
        return None
    return None


def diagnose_session(session: Session, max_line_bytes: int) -> Session:
    lines = 0
    max_line = 0
    oversized = 0
    with session.path.open("rb") as fh:
        for raw in fh:
            lines += 1
            size = len(raw.rstrip(b"\r\n"))
            max_line = max(max_line, size)
            if size > max_line_bytes:
                oversized += 1
    session.lines = lines
    session.max_line_bytes = max_line
    session.oversized_lines = oversized
    return session


def repair_session(
    session: Session,
    *,
    max_line_bytes: int,
    max_string_bytes: int,
    apply: bool,
) -> dict[str, Any]:
    tmp_path = session.path.with_suffix(session.path.suffix + ".repair-tmp")
    backup_path = session.path.with_name(
        f"{session.path.name}.bak-codex-session-repair-{time.strftime('%Y%m%dT%H%M%S')}"
    )

    stats = {
        "oversized_lines": 0,
        "invalid_json_lines": 0,
        "sanitized_strings": 0,
        "bytes_before": 0,
        "bytes_after": 0,
        "backup": str(backup_path),
    }

    out_fh = None
    try:
        if apply:
            out_fh = tmp_path.open("w", encoding="utf-8", newline="\n")
        with session.path.open("r", encoding="utf-8", errors="replace") as in_fh:
            for line in in_fh:
                had_newline = line.endswith("\n")
                raw = line.rstrip("\r\n")
                before = len(line.encode("utf-8", errors="replace"))
                stats["bytes_before"] += before

                if len(raw.encode("utf-8", errors="replace")) <= max_line_bytes:
                    repaired_line = line
                else:
                    stats["oversized_lines"] += 1
                    try:
                        value = json.loads(raw)
                        changed = sanitize_value(value, max_string_bytes)
                        stats["sanitized_strings"] += changed
                        repaired_line = json.dumps(value, ensure_ascii=False, separators=(",", ":"))
                    except json.JSONDecodeError:
                        stats["invalid_json_lines"] += 1
                        repaired_line = json.dumps(
                            {
                                "type": "event_msg",
                                "payload": {
                                    "type": "session_repair",
                                    "message": (
                                        f"{REPAIR_MARKER} replaced invalid oversized JSONL "
                                        f"line of {before} bytes"
                                    ),
                                },
                            },
                            ensure_ascii=False,
                            separators=(",", ":"),
                        )
                    if had_newline:
                        repaired_line += "\n"

                stats["bytes_after"] += len(repaired_line.encode("utf-8"))
                if out_fh:
                    out_fh.write(repaired_line)
    finally:
        if out_fh:
            out_fh.close()

    if apply:
        if stats["oversized_lines"] == 0:
            tmp_path.unlink(missing_ok=True)
        else:
            shutil.copy2(session.path, backup_path)
            tmp_path.replace(session.path)
    return stats


def sanitize_container(value: Any, max_string_bytes: int) -> tuple[Any, int]:
    if isinstance(value, str):
        return sanitize_string(value, max_string_bytes)
    if isinstance(value, list):
        changed = 0
        out = []
        for item in value:
            new_item, count = sanitize_container(item, max_string_bytes)
            out.append(new_item)
            changed += count
        return out, changed
    if isinstance(value, dict):
        changed = 0
        out = {}
        for key, item in value.items():
            new_item, count = sanitize_container(item, max_string_bytes)
            out[key] = new_item
            changed += count
        return out, changed
    return value, 0


def sanitize_value(value: Any, max_string_bytes: int) -> int:
    replacement, changed = sanitize_container(value, max_string_bytes)
    if isinstance(value, dict) and isinstance(replacement, dict):
        value.clear()
        value.update(replacement)
    elif isinstance(value, list) and isinstance(replacement, list):
        value[:] = replacement
    return changed


def sanitize_string(text: str, max_string_bytes: int) -> tuple[str, int]:
    encoded = text.encode("utf-8", errors="replace")
    if len(encoded) <= max_string_bytes and not text.startswith("data:image/"):
        return text, 0

    keep = min(max_string_bytes, 8192)
    head = utf8_prefix_bytes(text, keep // 2)
    tail = utf8_suffix_bytes(text, keep // 2)
    omitted = max(0, len(encoded) - len(head.encode("utf-8")) - len(tail.encode("utf-8")))
    return (
        f"{head}\n{REPAIR_MARKER} omitted {omitted} oversized bytes from this field\n{tail}",
        1,
    )


def utf8_prefix_bytes(text: str, byte_limit: int) -> str:
    out = text.encode("utf-8", errors="replace")[:byte_limit]
    return out.decode("utf-8", errors="ignore")


def utf8_suffix_bytes(text: str, byte_limit: int) -> str:
    out = text.encode("utf-8", errors="replace")[-byte_limit:]
    return out.decode("utf-8", errors="ignore")


def normalize_path(path: str | None) -> str:
    return str(Path(path or "").expanduser()).rstrip("/")


def one_line(text: str, max_chars: int) -> str:
    compact = " ".join(text.split())
    if len(compact) <= max_chars:
        return compact
    return compact[: max_chars - 1] + "…"


def format_bytes(value: int) -> str:
    units = ["B", "KB", "MB", "GB"]
    size = float(value)
    for unit in units:
        if size < 1024 or unit == units[-1]:
            return f"{size:.1f}{unit}" if unit != "B" else f"{int(size)}B"
        size /= 1024
    return f"{value}B"


def print_sessions(sessions: list[Session], limit: int) -> None:
    print("Codex sessions:")
    for idx, session in enumerate(sessions[:limit], start=1):
        title = session.title or "(untitled)"
        age = time.strftime("%Y-%m-%d %H:%M", time.localtime(session.updated))
        print(
            f"{idx:>3}. {title} | {format_bytes(session.bytes_size):>8} | "
            f"{age} | {session.session_id[:8]} | {session.path}"
        )


def choose_session(sessions: list[Session], limit: int) -> Session:
    while True:
        try:
            raw = input(f"Chọn session cần fix [1-{min(limit, len(sessions))}] hoặc q: ").strip()
        except EOFError:
            raise SystemExit(0)
        if raw.lower() in {"q", "quit", "exit"}:
            raise SystemExit(0)
        try:
            idx = int(raw)
        except ValueError:
            print("Nhập số trong danh sách.")
            continue
        if 1 <= idx <= min(limit, len(sessions)):
            return sessions[idx - 1]
        print("Số không hợp lệ.")


def main() -> int:
    parser = argparse.ArgumentParser(description="List and repair local Codex session JSONL files.")
    parser.add_argument("--codex-home", help="Default: CODEX_HOME or ~/.codex")
    parser.add_argument("--repo", help="Only show sessions for this cwd/repo path")
    parser.add_argument("--limit", type=int, default=30)
    parser.add_argument("--max-line-bytes", type=int, default=DEFAULT_MAX_LINE_BYTES)
    parser.add_argument("--max-string-bytes", type=int, default=DEFAULT_MAX_STRING_BYTES)
    parser.add_argument("--session-id", help="Repair this session id without interactive selection")
    parser.add_argument("--list", action="store_true", help="Only list sessions; do not prompt")
    parser.add_argument("--dry-run", action="store_true", help="Diagnose only; do not write")
    parser.add_argument("--yes", action="store_true", help="Do not prompt before writing")
    args = parser.parse_args()

    codex_home = codex_home_from_args(args.codex_home)
    sessions = discover_sessions(codex_home, args.repo)
    if not sessions:
        print(f"No Codex sessions found under {codex_home}", file=sys.stderr)
        return 1

    if args.session_id:
        selected = next((s for s in sessions if s.session_id == args.session_id), None)
        if selected is None:
            print(f"Session not found: {args.session_id}", file=sys.stderr)
            return 1
    else:
        print_sessions(sessions, args.limit)
        if args.list:
            return 0
        selected = choose_session(sessions, args.limit)

    diagnose_session(selected, args.max_line_bytes)
    print()
    print(f"Selected: {selected.title or '(untitled)'}")
    print(f"ID:       {selected.session_id}")
    print(f"Path:     {selected.path}")
    print(f"Size:     {format_bytes(selected.bytes_size)}")
    print(f"Lines:    {selected.lines}")
    print(f"Max line: {format_bytes(selected.max_line_bytes)}")
    print(f"Large:    {selected.oversized_lines} line(s) over {format_bytes(args.max_line_bytes)}")

    if selected.oversized_lines == 0:
        print("Nothing to repair.")
        return 0

    apply = not args.dry_run
    if apply and not args.yes:
        raw = input("Tạo backup và repair session này? [y/N] ").strip().lower()
        apply = raw in {"y", "yes"}
    stats = repair_session(
        selected,
        max_line_bytes=args.max_line_bytes,
        max_string_bytes=args.max_string_bytes,
        apply=apply,
    )

    print()
    print("Result:")
    print(f"Mode:              {'apply' if apply else 'dry-run'}")
    print(f"Oversized lines:   {stats['oversized_lines']}")
    print(f"Sanitized strings: {stats['sanitized_strings']}")
    print(f"Before:            {format_bytes(stats['bytes_before'])}")
    print(f"After:             {format_bytes(stats['bytes_after'])}")
    if apply and stats["oversized_lines"]:
        print(f"Backup:            {stats['backup']}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
