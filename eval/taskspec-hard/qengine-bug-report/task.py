DOMAIN = "qengine"
MUTATIONS = [
    ("qengine/parser.py", '''        left = self.multiplicative()
        while self.at("OP", "+") or self.at("OP", "-"):
            op = self.next().value
            left = Binary(op, left, self.multiplicative())
        return left''', '''        left = self.multiplicative()
        if self.at("OP", "+") or self.at("OP", "-"):
            op = self.next().value
            return Binary(op, left, self.additive())
        return left'''),
    ("qengine/executor.py", '''        if not 1 <= e.value <= len(names):
            raise EvalError(f"ORDER BY position {e.value} out of range")
        idx = e.value - 1''', '''        if not 0 <= e.value < len(names):
            raise EvalError(f"ORDER BY position {e.value} out of range")
        idx = e.value'''),
    ("qengine/executor.py", '''    """Stable multi-key sort: sort by the last key first, so earlier keys win."""
    for item in reversed(q.order_by):''', '''    """Stable multi-key sort: apply the keys one after another."""
    for item in q.order_by:'''),
    ("qengine/executor.py", "out[start:] if q.limit is None else out[start:start + q.limit]", "out[start:] if q.limit is None else out[start:q.limit]"),
    ("qengine/executor.py", '''    out = []
    for left in left_rows:
        matched = False
        for right in right_rows:''', '''    out = []
    matched = False
    for left in left_rows:
        for right in right_rows:'''),
]
