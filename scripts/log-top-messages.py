"""Top messages of a JSON-lines log, by target and message template.

Used by the memory plan's O5: the 2026-11-15 log holds 549 MB, and the first question
is which target and which message filled it. A template is the message with numbers,
paths, quoted values, hex and floating point replaced, so the per-item lines of one
loop collapse into a single row.
"""

import collections
import json
import pathlib
import re
import sys

# The floating point forms come first: the integer rule would eat their halves.
NUMBER = re.compile(r"-?\d+\.\d+(?:[eE][+-]?\d+)?")
INTEGER = re.compile(r"-?\b\d+\b")
PATH = re.compile(r"[A-Za-z]:\\[^\"',;\s\\]*(?:\\[^\"',;\s\\]*)*")
GUID = re.compile(r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b")
HEX = re.compile(r"\b0x[0-9a-fA-F]+\b")
QUOTED = re.compile(r'"[^"]*"')
SPACES = re.compile(r"\s+")


def template(message: str) -> str:
    text = QUOTED.sub("<str>", message)
    text = GUID.sub("<uuid>", text)
    text = HEX.sub("<hex>", text)
    text = PATH.sub("<path>", text)
    text = NUMBER.sub("<num>", text)
    text = INTEGER.sub("<n>", text)
    return SPACES.sub(" ", text).strip()


def main() -> None:
    path = pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else "userdata/state/logs/nohrs.log.2026-11-15")
    top = int(sys.argv[2]) if len(sys.argv) > 2 else 20
    counts: collections.Counter = collections.Counter()
    targets: collections.Counter = collections.Counter()
    levels: collections.Counter = collections.Counter()
    examples: dict = {}
    lines = 0
    broken = 0
    with path.open("r", encoding="utf-8", errors="replace") as handle:
        for line in handle:
            lines += 1
            try:
                record = json.loads(line)
            except json.JSONDecodeError:
                broken += 1
                continue
            target = record.get("target") or record.get("log.target") or "?"
            level = record.get("level", "?")
            message = str(record.get("message", ""))
            key = (target, template(message))
            counts[key] += 1
            targets[target] += 1
            levels[level] += 1
            examples.setdefault(key, message)

    print(f"file      {path}")
    print(f"lines     {lines:,}")
    print(f"unparsed  {broken:,}")
    print(f"template rows {len(counts):,}")
    print()
    print("levels")
    for level, count in levels.most_common():
        print(f"  {level:<6} {count:>12,}")
    print()
    print(f"top {top} by target and template")
    for (target, tpl), count in counts.most_common(top):
        print(f"{count:>12,}  {target}  |  {tpl}")
        print(f"              e.g. {examples[(target, tpl)][:200]}")
    print()
    print("top 10 targets by lines")
    for target, count in targets.most_common(10):
        print(f"  {count:>12,}  {target}")


if __name__ == "__main__":
    main()