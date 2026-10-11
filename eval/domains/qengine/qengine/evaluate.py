"""Expression evaluation against a row.

A row is a dict {(table_alias, column): value}; NULL is Python None.
"""
import operator

from .nodes import AGGREGATES, Binary, Call, Column, Literal, Unary


class EvalError(ValueError):
    pass


def _div(a, b):
    if b == 0:
        raise EvalError("division by zero")
    return a / b


def _mod(a, b):
    if b == 0:
        raise EvalError("division by zero")
    return a % b


def _add(a, b):
    if isinstance(a, str) or isinstance(b, str):
        if not (isinstance(a, str) and isinstance(b, str)):
            raise EvalError(f"cannot add {type(a).__name__} and {type(b).__name__}")
    return a + b


BINOPS = {
    "=": operator.eq, "!=": operator.ne, "<": operator.lt, "<=": operator.le, ">": operator.gt, ">=": operator.ge,
    "+": _add, "-": operator.sub, "*": operator.mul, "/": _div, "%": _mod,
}


def _coalesce(*args):
    for a in args:
        if a is not None:
            return a
    return None


FUNCTIONS = {
    "UPPER": (1, lambda s: s.upper()),
    "LOWER": (1, lambda s: s.lower()),
    "LENGTH": (1, len),
    "ABS": (1, abs),
    "ROUND": (2, lambda x, n: round(x, n)),
    "COALESCE": (None, _coalesce),
}


def truthy(v):
    """Whether a condition value keeps a row (WHERE / HAVING / ON)."""
    return bool(v)


def resolve(row, col):
    if col.table is not None:
        key = (col.table, col.name)
        if key not in row:
            raise EvalError(f"unknown column {col.table}.{col.name}")
        return row[key]
    hits = [k for k in row if k[1] == col.name]
    if not hits:
        raise EvalError(f"unknown column {col.name}")
    if len(hits) > 1:
        raise EvalError(f"ambiguous column {col.name}")
    return row[hits[0]]


def evaluate(e, row, aggs=None):
    """aggs maps aggregate Call nodes to their value for the current group (None outside grouping)."""
    t = type(e)
    if t is Literal:
        return e.value
    if t is Column:
        return resolve(row, e)
    if t is Unary:
        v = evaluate(e.operand, row, aggs)
        if e.op == "NOT":
            return not truthy(v)
        return -v
    if t is Binary:
        if e.op == "AND":
            return truthy(evaluate(e.left, row, aggs)) and truthy(evaluate(e.right, row, aggs))
        if e.op == "OR":
            return truthy(evaluate(e.left, row, aggs)) or truthy(evaluate(e.right, row, aggs))
        left, right = evaluate(e.left, row, aggs), evaluate(e.right, row, aggs)
        try:
            return BINOPS[e.op](left, right)
        except TypeError:
            raise EvalError(f"bad operands for {e.op}: {left!r}, {right!r}") from None
    if t is Call:
        if e.name in AGGREGATES:
            if aggs is None or e not in aggs:
                raise EvalError(f"aggregate {e.name} not allowed here")
            return aggs[e]
        if e.name not in FUNCTIONS:
            raise EvalError(f"unknown function {e.name}")
        arity, fn = FUNCTIONS[e.name]
        if arity is not None and len(e.args) != arity:
            raise EvalError(f"{e.name} takes {arity} argument(s)")
        args = [evaluate(a, row, aggs) for a in e.args]
        try:
            return fn(*args)
        except (TypeError, AttributeError):
            raise EvalError(f"bad argument for {e.name}: {args!r}") from None
    raise EvalError(f"cannot evaluate {e!r}")
