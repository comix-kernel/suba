"""Report physical source lines across all workspace runtime crates."""
from collections import Counter
import json
import os
from pathlib import Path
import subprocess

root = Path(__file__).resolve().parents[1] / "os"
packages = json.loads(subprocess.check_output(
    ["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"],
    cwd=root))["packages"]
allowed = {"service-api": set(), "components": {"service-api"}, "directory": {"service-api"}}
for package in packages:
    if package["name"] in allowed:
        dependencies = {dep["name"] for dep in package["dependencies"] if dep["kind"] is None}
        if dependencies != allowed[package["name"]]:
            raise SystemExit(f"Unexpected dependencies for {package['name']}: {dependencies}")

lines = Counter()
for directory, dirs, files in os.walk(root):
    dirs[:] = [d for d in dirs if d not in {"target", "vendor", "tests", ".git"}]
    for name in files:
        path = Path(directory, name)
        if path.suffix in {".rs", ".S", ".s", ".ld"} and name != "build.rs":
            lines[str(path.parent.relative_to(root))] += len(path.read_bytes().splitlines())
for directory, count in sorted(lines.items()):
    print(f"{directory:20s} {count:5d}")
total = sum(lines.values())
print(f"runtime source: {total} lines (including comments and blanks)")
