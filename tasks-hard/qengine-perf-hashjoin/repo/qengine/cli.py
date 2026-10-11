"""Usage: python3 -m qengine.cli DATA_DIR "SELECT ..."   (every DATA_DIR/<name>.csv becomes table <name>)"""
import sys

from .catalog import CatalogError, Database
from .evaluate import EvalError
from .executor import execute
from .format import render
from .lexer import LexError
from .parser import ParseError


def main(argv=None):
    argv = sys.argv[1:] if argv is None else argv
    if len(argv) != 2:
        print(__doc__.strip(), file=sys.stderr)
        return 2
    db = Database()
    db.load_dir(argv[0])
    try:
        print(render(execute(db, argv[1])))
    except (LexError, ParseError, EvalError, CatalogError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
