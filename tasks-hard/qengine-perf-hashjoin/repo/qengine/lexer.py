"""Tokenizer for the qengine SQL dialect."""
from dataclasses import dataclass

KEYWORDS = {
    "SELECT", "DISTINCT", "FROM", "WHERE", "GROUP", "BY", "HAVING", "ORDER", "ASC", "DESC", "LIMIT", "OFFSET",
    "AND", "OR", "NOT", "AS", "JOIN", "INNER", "LEFT", "ON", "NULL", "TRUE", "FALSE",
}
_OPS2 = ("<=", ">=", "!=", "<>")
_OPS1 = "=<>+-*/%(),."


class LexError(ValueError):
    pass


@dataclass(frozen=True)
class Token:
    kind: str  # NUM, STR, IDENT, KW, OP, EOF
    value: object
    pos: int


def tokenize(src):
    out, i, n = [], 0, len(src)
    while i < n:
        c = src[i]
        if c.isspace():
            i += 1
            continue
        if src.startswith("--", i):  # comment to end of line
            j = src.find("\n", i)
            i = n if j < 0 else j
            continue
        if c.isdigit() or (c == "." and i + 1 < n and src[i + 1].isdigit()):
            j = i
            while j < n and (src[j].isdigit() or src[j] == "."):
                j += 1
            text = src[i:j]
            if text.count(".") > 1:
                raise LexError(f"bad number {text!r} at {i}")
            out.append(Token("NUM", float(text) if "." in text else int(text), i))
            i = j
            continue
        if c == "'":
            j, buf = i + 1, []
            while True:
                if j >= n:
                    raise LexError(f"unterminated string at {i}")
                if src[j] == "'":
                    if j + 1 < n and src[j + 1] == "'":  # '' is an escaped quote
                        buf.append("'")
                        j += 2
                        continue
                    break
                buf.append(src[j])
                j += 1
            out.append(Token("STR", "".join(buf), i))
            i = j + 1
            continue
        if c.isalpha() or c == "_":
            j = i
            while j < n and (src[j].isalnum() or src[j] == "_"):
                j += 1
            word = src[i:j]
            out.append(Token("KW", word.upper(), i) if word.upper() in KEYWORDS else Token("IDENT", word, i))
            i = j
            continue
        if src[i:i + 2] in _OPS2:
            out.append(Token("OP", src[i:i + 2], i))
            i += 2
            continue
        if c in _OPS1:
            out.append(Token("OP", c, i))
            i += 1
            continue
        raise LexError(f"unexpected {c!r} at {i}")
    out.append(Token("EOF", None, n))
    return out
