import sys

from .filters import by_component, by_level
from .parser import read_records
from .render import summary_text


def main(argv):
    if not argv:
        print("usage: logkit FILE [--level L] [--component C] [--summary]", file=sys.stderr)
        return 2
    path, opts = argv[0], argv[1:]
    records = read_records(path)
    i = 0
    summary = False
    while i < len(opts):
        if opts[i] == "--level":
            records = by_level(records, opts[i + 1])
            i += 2
        elif opts[i] == "--component":
            records = by_component(records, opts[i + 1])
            i += 2
        elif opts[i] == "--summary":
            summary = True
            i += 1
        else:
            print(f"unknown option: {opts[i]}", file=sys.stderr)
            return 2
    if summary:
        print(summary_text(records))
    else:
        for r in records:
            print(r["raw"])
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
