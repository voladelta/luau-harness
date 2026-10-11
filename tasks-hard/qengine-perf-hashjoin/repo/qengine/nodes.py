"""AST for queries. Expression nodes are frozen (hashable) so they can key per-group aggregate results."""
from dataclasses import dataclass, field

AGGREGATES = {"COUNT", "SUM", "AVG", "MIN", "MAX"}


@dataclass(frozen=True)
class Literal:
    value: object


@dataclass(frozen=True)
class Column:
    name: str
    table: str | None = None


@dataclass(frozen=True)
class Star:
    table: str | None = None


@dataclass(frozen=True)
class Unary:
    op: str  # "-", "NOT"
    operand: object


@dataclass(frozen=True)
class Binary:
    op: str  # "AND", "OR", "=", "!=", "<", "<=", ">", ">=", "+", "-", "*", "/", "%"
    left: object
    right: object


@dataclass(frozen=True)
class Call:
    name: str
    args: tuple = ()
    distinct: bool = False
    star: bool = False  # COUNT(*)


@dataclass
class SelectItem:
    expr: object
    alias: str | None = None


@dataclass
class Join:
    kind: str  # "INNER" or "LEFT"
    table: str
    alias: str | None
    on: object


@dataclass
class OrderItem:
    expr: object
    desc: bool = False


@dataclass
class Select:
    items: list
    table: str
    alias: str | None = None
    joins: list = field(default_factory=list)
    where: object = None
    group_by: list = field(default_factory=list)
    having: object = None
    order_by: list = field(default_factory=list)
    limit: int | None = None
    offset: int | None = None
    distinct: bool = False


def children(e):
    if isinstance(e, Unary):
        return (e.operand,)
    if isinstance(e, Binary):
        return (e.left, e.right)
    if isinstance(e, Call):
        return e.args
    return ()


def walk(e):
    yield e
    for c in children(e):
        yield from walk(c)


def is_aggregate(e):
    return isinstance(e, Call) and e.name in AGGREGATES


def aggregates_in(e):
    """Aggregate calls in e, outermost only, in first-seen order."""
    if e is None:
        return []
    if is_aggregate(e):
        return [e]
    out = []
    for c in children(e):
        for a in aggregates_in(c):
            if a not in out:
                out.append(a)
    return out


def display_name(e):
    """Default output column name for an unaliased select expression."""
    if isinstance(e, Column):
        return e.name
    if isinstance(e, Call):
        inner = "*" if e.star else ", ".join(display_name(a) for a in e.args)
        return f"{e.name.lower()}({'distinct ' if e.distinct else ''}{inner})"
    if isinstance(e, Literal):
        return "NULL" if e.value is None else repr(e.value)
    if isinstance(e, Unary):
        return f"{e.op.lower()} {display_name(e.operand)}" if e.op == "NOT" else f"-{display_name(e.operand)}"
    if isinstance(e, Binary):
        return f"{display_name(e.left)} {e.op.lower()} {display_name(e.right)}"
    return "?"
