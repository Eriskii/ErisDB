#!/usr/bin/env python3
"""Compare core/DB CLOC against a Git revision, excluding inline unit tests.

Use --format to run the same rustfmt settings on both temporary snapshots,
so line wrapping cannot account for the reported production-code reduction.
The working tree is never formatted or otherwise modified.
"""

import argparse
import json
from pathlib import Path
import subprocess
import tempfile


ROOT = Path(__file__).resolve().parents[1]
SCOPES = ("erisdb/src", "erisdb/migrations")


def command(*args):
    return subprocess.check_output(args, cwd=ROOT, text=True)


def count(directory):
    report = json.loads(command("cloc", "--quiet", "--json", str(directory)))
    return report["SUM"]["code"]


def measure(files, directory, formatting):
    for name, source in files:
        path = directory / name
        path.parent.mkdir(parents=True, exist_ok=True)
        # Core inline tests are trailing #[cfg(test)] modules. Count their
        # replacements separately rather than claiming they simplified runtime.
        if path.suffix == ".rs":
            source = source.split("#[cfg(test)]", 1)[0]
        path.write_text(source)
    if formatting:
        paths = sorted(directory.rglob("*.rs"))
        command("rustfmt", "--edition", "2021", "--config",
                "max_width=120,skip_children=true", *map(str, paths))
    return count(directory)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", default="HEAD")
    parser.add_argument("--format", action="store_true")
    args = parser.parse_args()
    base = command("git", "rev-parse", args.base).strip()
    names = command("git", "ls-tree", "-r", "--name-only", base, "--", *SCOPES).splitlines()
    before = [(name, command("git", "show", f"{base}:{name}")) for name in names
              if Path(name).suffix in (".rs", ".sql")]
    after = [(str(path.relative_to(ROOT)), path.read_text())
             for scope in SCOPES for path in sorted((ROOT / scope).rglob("*"))
             if path.suffix in (".rs", ".sql")]
    with tempfile.TemporaryDirectory(prefix="erisdb-cloc-") as temporary:
        directory = Path(temporary)
        old = measure(before, directory / "before", args.format)
        new = measure(after, directory / "after", args.format)
    print(json.dumps({"base": base, "scope": SCOPES, "inline_tests": "excluded",
                      "normalized_format": args.format, "before": old, "after": new,
                      "reduction": old - new, "percent": round(100 * (old - new) / old, 1)}, indent=2))


if __name__ == "__main__":
    main()
