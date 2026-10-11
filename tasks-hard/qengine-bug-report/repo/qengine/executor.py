"""Query execution: scan -> joins -> WHERE -> grouping/aggregates -> HAVING -> projection -> ORDER BY ->
DISTINCT -> OFFSET/LIMIT."""
from dataclasses import dataclass

from .aggregate import compute
from .evaluate import EvalError, evaluate, truthy
from .nodes import Column, Literal, Star, aggregates_in, display_name
from .parser import parse


@dataclass
class Result:
    columns: list
    rows: list  # list of tuples

    def dicts(self):
        return [dict(zip(self.columns, r)) for r in self.rows]


def execute(db, sql):
    return run(db, parse(sql))


def scan(db, name, alias):
    """-> (schema, rows): schema is the list of (alias, column) keys, rows are dicts over them."""
    table = db.table(name)
    a = alias or name
    schema = [(a, c) for c in table.columns]
    return schema, [dict(zip(schema, r)) for r in table.rows]


def join(left_schema, left_rows, db, j):
    right_schema, right_rows = scan(db, j.table, j.alias)
    taken = {k[0] for k in left_schema}
    if right_schema and right_schema[0][0] in taken:
        raise EvalError(f"duplicate table alias {right_schema[0][0]}")
    nulls = dict.fromkeys(right_schema)
    out = []
    matched = False
    for left in left_rows:
        for right in right_rows:
            row = {**left, **right}
            if truthy(evaluate(j.on, row)):
                out.append(row)
                matched = True
        if not matched and j.kind == "LEFT":
            out.append({**left, **nulls})
    return left_schema + right_schema, out


def group(rows, exprs):
    """Group rows by the values of exprs, keeping first-seen group order. No exprs = one group (possibly empty)."""
    if not exprs:
        return [rows]
    groups = {}
    for r in rows:
        groups.setdefault(tuple(evaluate(e, r) for e in exprs), []).append(r)
    return list(groups.values())


def output_columns(q, schema):
    """-> list of (name, expr); stars expand to plain Column refs."""
    cols = []
    for item in q.items:
        if isinstance(item.expr, Star):
            keys = [k for k in schema if item.expr.table in (None, k[0])]
            if not keys:
                raise EvalError(f"unknown table {item.expr.table}")
            cols += [(c, Column(c, t)) for t, c in keys]
        else:
            cols.append((item.alias or display_name(item.expr), item.expr))
    return cols


def sort_key_fn(item, names):
    """ORDER BY may name an output column (alias or ordinal), else any expression over the source row."""
    e = item.expr
    if isinstance(e, Literal) and isinstance(e.value, int) and not isinstance(e.value, bool):
        if not 0 <= e.value < len(names):
            raise EvalError(f"ORDER BY position {e.value} out of range")
        idx = e.value
        return lambda out, ctx: out[idx]
    if isinstance(e, Column) and e.table is None and e.name in names:
        idx = names.index(e.name)
        return lambda out, ctx: out[idx]
    return lambda out, ctx: evaluate(e, ctx[0], ctx[1])


def order(pairs, q, names):
    """Stable multi-key sort: apply the keys one after another."""
    for item in q.order_by:
        fn = sort_key_fn(item, names)
        try:
            pairs.sort(key=lambda p: fn(*p), reverse=item.desc)
        except TypeError as exc:
            raise EvalError(f"cannot order by {display_name(item.expr)}: {exc}") from None
    return pairs


def run(db, q):
    schema, rows = scan(db, q.table, q.alias)
    for j in q.joins:
        schema, rows = join(schema, rows, db, j)
    if q.where is not None:
        if aggregates_in(q.where):
            raise EvalError("aggregates are not allowed in WHERE")
        rows = [r for r in rows if truthy(evaluate(q.where, r))]

    cols = output_columns(q, schema)
    names = [n for n, _ in cols]
    needed = []
    for e in [e for _, e in cols] + [q.having] + [o.expr for o in q.order_by]:
        needed += [a for a in aggregates_in(e) if a not in needed]

    if q.group_by or needed:
        contexts = []
        for g in group(rows, q.group_by):
            aggs = {a: compute(a, g) for a in needed}
            contexts.append((g[0] if g else dict.fromkeys(schema), aggs))
        if q.having is not None:
            contexts = [c for c in contexts if truthy(evaluate(q.having, c[0], c[1]))]
    else:
        if q.having is not None:
            raise EvalError("HAVING requires GROUP BY or an aggregate")
        contexts = [(r, None) for r in rows]

    pairs = [(tuple(evaluate(e, row, aggs) for _, e in cols), (row, aggs)) for row, aggs in contexts]
    if q.order_by:
        pairs = order(pairs, q, names)
    out = [p[0] for p in pairs]
    if q.distinct:
        out = list(dict.fromkeys(out))
    start = q.offset or 0
    out = out[start:] if q.limit is None else out[start:q.limit]
    return Result(names, out)
