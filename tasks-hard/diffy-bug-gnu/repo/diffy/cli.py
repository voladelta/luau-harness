"""Usage:
  python3 -m diffy.cli diff OLD NEW [-U N]     print a unified diff
  python3 -m diffy.cli apply PATCH FILE        apply PATCH to FILE in place
"""
import sys

from .apply import PatchError, apply
from .parse import ParseError, parse_patch
from .unified import unified


def read_lines(path):
    with open(path) as f:
        return f.read().splitlines()


def main(argv=None):
    argv = sys.argv[1:] if argv is None else argv
    if len(argv) >= 3 and argv[0] == "diff":
        context = int(argv[argv.index("-U") + 1]) if "-U" in argv else 3
        sys.stdout.write(unified(read_lines(argv[1]), read_lines(argv[2]), argv[1], argv[2], context))
        return 0
    if len(argv) == 3 and argv[0] == "apply":
        with open(argv[1]) as f:
            text = f.read()
        try:
            out = apply(parse_patch(text), read_lines(argv[2]))
        except (ParseError, PatchError) as exc:
            print(f"error: {exc}", file=sys.stderr)
            return 1
        with open(argv[2], "w") as f:
            f.write("".join(line + "\n" for line in out))
        return 0
    print(__doc__.strip(), file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main())
