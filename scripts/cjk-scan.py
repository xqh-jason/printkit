#!/usr/bin/env python3
"""Fail when repository files outside the allowed exceptions contain CJK text."""
import os
import re
import sys

PATTERN = re.compile(r"[\u4e00-\u9fff]")
SKIP_DIRS = {
    ".git", "target", "_work", "fixtures", "baseline", "node_modules", ".mimosa",
    ".archify", "dist", "evidence", "assets",
    "apps/desktop/src-tauri/binaries", "apps/desktop/src-tauri/icons",
}
TEXT_SUFFIXES = (".rs", ".toml", ".md", ".sh", ".py", ".yml", ".yaml", ".json",
                 ".ts", ".mjs", ".js", ".html", ".css", ".txt", ".sql", ".jq")
ALLOWED = {"README.zh-CN.md", "docs/web-integration.zh-CN.md", "docs/cli.zh-CN.md"}
ALLOWED_PREFIXES = ("packages/i18n/",)


def main() -> int:
    found = []
    for root, dirs, files in os.walk("."):
        dirs[:] = [d for d in dirs if d not in SKIP_DIRS]
        for name in files:
            path = os.path.join(root, name)
            rel = os.path.relpath(path, ".")
            if rel in ALLOWED or rel.startswith(ALLOWED_PREFIXES) or not name.endswith(TEXT_SUFFIXES):
                continue
            try:
                text = open(path, "rb").read().decode("utf-8", "ignore")
            except OSError:
                continue
            if PATTERN.search(text):
                found.append(path)
    if found:
        print("\n".join(found))
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
