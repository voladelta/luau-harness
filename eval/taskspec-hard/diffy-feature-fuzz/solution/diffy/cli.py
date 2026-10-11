"""Usage:
  python3 -m diffy.cli diff OLD NEW [-U N]     print a unified diff
  python3 -m diffy.cli apply PATCH FILE [--fuzz N]
                                               apply PATCH to FILE in place; failed hunks go to FILE.rej
"""
import sys

from .apply import apply_fuzzy, report
from .parse import ParseError, parse_patch
from .unified import format_hunk, unified


def read_lines(path):
    with open(path) as f:
        return f.read().splitlines()


def main(argv=None):
    argv = sys.argv[1:] if argv is None else argv
    if len(argv) >= 3 and argv[0] == "diff":
        context = int(argv[argv.index("-U") + 1]) if "-U" in argv else 3
        sys.stdout.write(unified(read_lines(argv[1]), read_lines(argv[2]), argv[1], argv[2], context))
        return 0
    if len(argv) in (3, 5) and argv[0] == "apply" and (len(argv) == 3 or argv[3] == "--fuzz"):
        fuzz = int(argv[4]) if len(argv) == 5 else 2
        with open(argv[1]) as f:
            text = f.read()
        try:
            patch = parse_patch(text)
        except ParseError as exc:
            print(f"error: {exc}", file=sys.stderr)
            return 1
        result = apply_fuzzy(patch, read_lines(argv[2]), fuzz)
        for msg in report(result):
            print(msg)
        with open(argv[2], "w") as f:
            f.write("".join(line + "\n" for line in result.lines))
        if result.failed:
            rej = [f"--- {patch.old_name}", f"+++ {patch.new_name}"]
            for i in result.failed:
                rej += format_hunk(patch.hunks[i])
            with open(argv[2] + ".rej", "w") as f:
                f.write("\n".join(rej) + "\n")
            return 1
        return 0
    print(__doc__.strip(), file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main())
