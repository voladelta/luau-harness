"""Aggregate functions over the rows of one group."""
from .evaluate import EvalError, evaluate


def compute(call, rows):
    if call.star:
        return len(rows)
    if len(call.args) != 1:
        raise EvalError(f"{call.name} takes exactly one argument")
    values = [v for v in (evaluate(call.args[0], r) for r in rows) if v is not None]
    if call.distinct:
        values = list(dict.fromkeys(values))
    if call.name == "COUNT":
        return len(values)
    if call.name == "SUM":
        return sum(values) if values else None
    if call.name == "AVG":
        return sum(values) / len(values) if values else None
    if call.name == "MIN":
        return min(values) if values else None
    if call.name == "MAX":
        return max(values) if values else None
    raise EvalError(f"unknown aggregate {call.name}")
