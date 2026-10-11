"""Recursive-descent parser: SQL text -> nodes.Select.

Expression precedence, loosest first: OR, AND, NOT, comparison, + -, * / %, unary minus, primary.
"""
from .lexer import tokenize
from .nodes import Binary, Call, Column, Join, Literal, OrderItem, Select, SelectItem, Star, Unary

_CMP = {"=": "=", "!=": "!=", "<>": "!=", "<": "<", "<=": "<=", ">": ">", ">=": ">="}


class ParseError(ValueError):
    pass


class Parser:
    def __init__(self, tokens):
        self.toks = tokens
        self.i = 0

    # -- token helpers
    def peek(self, k=0):
        return self.toks[min(self.i + k, len(self.toks) - 1)]

    def next(self):
        t = self.toks[self.i]
        self.i += 1
        return t

    def at(self, kind, value=None):
        t = self.peek()
        return t.kind == kind and (value is None or t.value == value)

    def accept(self, kind, value=None):
        return self.next() if self.at(kind, value) else None

    def expect(self, kind, value=None):
        if not self.at(kind, value):
            t = self.peek()
            raise ParseError(f"expected {value or kind} at {t.pos}, got {t.value if t.kind != 'EOF' else 'end of input'!r}")
        return self.next()

    def kw(self, word):
        return self.accept("KW", word)

    # -- statements
    def select(self):
        self.expect("KW", "SELECT")
        q = Select(items=[], table="")
        q.distinct = bool(self.kw("DISTINCT"))
        q.items = [self.select_item()]
        while self.accept("OP", ","):
            q.items.append(self.select_item())
        self.expect("KW", "FROM")
        q.table, q.alias = self.table_ref()
        while self.at("KW", "JOIN") or self.at("KW", "INNER") or self.at("KW", "LEFT"):
            kind = "LEFT" if self.kw("LEFT") else "INNER"
            self.kw("INNER")
            self.expect("KW", "JOIN")
            table, alias = self.table_ref()
            self.expect("KW", "ON")
            q.joins.append(Join(kind, table, alias, self.expr()))
        if self.kw("WHERE"):
            q.where = self.expr()
        if self.kw("GROUP"):
            self.expect("KW", "BY")
            q.group_by = self.expr_list()
        if self.kw("HAVING"):
            q.having = self.expr()
        if self.kw("ORDER"):
            self.expect("KW", "BY")
            q.order_by = [self.order_item()]
            while self.accept("OP", ","):
                q.order_by.append(self.order_item())
        if self.kw("LIMIT"):
            q.limit = self.count()
        if self.kw("OFFSET"):
            q.offset = self.count()
        self.expect("EOF")
        return q

    def count(self):
        t = self.expect("NUM")
        if not isinstance(t.value, int) or t.value < 0:
            raise ParseError(f"expected a non-negative integer at {t.pos}")
        return t.value

    def table_ref(self):
        name = self.expect("IDENT").value
        self.kw("AS")
        alias = self.accept("IDENT")
        return name, (alias.value if alias else None)

    def select_item(self):
        if self.accept("OP", "*"):
            return SelectItem(Star())
        if self.at("IDENT") and self.peek(1).value == "." and self.peek(2).kind == "OP" and self.peek(2).value == "*":
            table = self.next().value
            self.next(), self.next()
            return SelectItem(Star(table))
        e = self.expr()
        if self.kw("AS"):
            return SelectItem(e, self.expect("IDENT").value)
        if self.at("IDENT"):
            return SelectItem(e, self.next().value)
        return SelectItem(e)

    def order_item(self):
        e = self.expr()
        if self.kw("DESC"):
            return OrderItem(e, True)
        self.kw("ASC")
        return OrderItem(e, False)

    def expr_list(self):
        out = [self.expr()]
        while self.accept("OP", ","):
            out.append(self.expr())
        return out

    # -- expressions
    def expr(self):
        return self.or_expr()

    def or_expr(self):
        left = self.and_expr()
        while self.kw("OR"):
            left = Binary("OR", left, self.and_expr())
        return left

    def and_expr(self):
        left = self.not_expr()
        while self.kw("AND"):
            left = Binary("AND", left, self.not_expr())
        return left

    def not_expr(self):
        if self.kw("NOT"):
            return Unary("NOT", self.not_expr())
        return self.comparison()

    def comparison(self):
        left = self.additive()
        t = self.peek()
        if t.kind == "OP" and t.value in _CMP:
            self.next()
            return Binary(_CMP[t.value], left, self.additive())
        return left

    def additive(self):
        left = self.multiplicative()
        while self.at("OP", "+") or self.at("OP", "-"):
            op = self.next().value
            left = Binary(op, left, self.multiplicative())
        return left

    def multiplicative(self):
        left = self.unary()
        while self.at("OP", "*") or self.at("OP", "/") or self.at("OP", "%"):
            op = self.next().value
            left = Binary(op, left, self.unary())
        return left

    def unary(self):
        if self.accept("OP", "-"):
            return Unary("-", self.unary())
        if self.accept("OP", "+"):
            return self.unary()
        return self.primary()

    def primary(self):
        t = self.next()
        if t.kind in ("NUM", "STR"):
            return Literal(t.value)
        if t.kind == "KW" and t.value in ("NULL", "TRUE", "FALSE"):
            return Literal({"NULL": None, "TRUE": True, "FALSE": False}[t.value])
        if t.kind == "OP" and t.value == "(":
            e = self.expr()
            self.expect("OP", ")")
            return e
        if t.kind == "IDENT":
            if self.at("OP", "("):
                return self.call(t.value.upper())
            if self.accept("OP", "."):
                return Column(self.expect("IDENT").value, t.value)
            return Column(t.value)
        raise ParseError(f"unexpected {t.value if t.kind != 'EOF' else 'end of input'!r} at {t.pos}")

    def call(self, name):
        self.expect("OP", "(")
        if self.accept("OP", "*"):
            self.expect("OP", ")")
            if name != "COUNT":
                raise ParseError(f"{name}(*) is not supported")
            return Call(name, (), star=True)
        distinct = bool(self.kw("DISTINCT"))
        args = [] if self.at("OP", ")") else self.expr_list()
        self.expect("OP", ")")
        return Call(name, tuple(args), distinct)


def parse(sql):
    return Parser(tokenize(sql)).select()
