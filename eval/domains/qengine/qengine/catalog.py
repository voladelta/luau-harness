"""In-memory tables. Rows are tuples in column order; NULL is None."""
import csv
from pathlib import Path


class CatalogError(ValueError):
    pass


class Table:
    def __init__(self, name, columns, rows=()):
        if len(set(columns)) != len(columns):
            raise CatalogError(f"duplicate column in {name}")
        self.name = name
        self.columns = list(columns)
        self.rows = []
        for r in rows:
            self.insert(r)

    def insert(self, row):
        row = tuple(row)
        if len(row) != len(self.columns):
            raise CatalogError(f"{self.name}: expected {len(self.columns)} values, got {len(row)}")
        self.rows.append(row)


class Database:
    def __init__(self):
        self.tables = {}

    def create(self, name, columns, rows=()):
        if name in self.tables:
            raise CatalogError(f"table {name} already exists")
        self.tables[name] = Table(name, columns, rows)
        return self.tables[name]

    def table(self, name):
        try:
            return self.tables[name]
        except KeyError:
            raise CatalogError(f"no such table: {name}") from None

    def load_csv(self, name, path):
        """Header row = column names. Empty cells are NULL; ints and floats are parsed."""
        with open(path, newline="") as f:
            reader = csv.reader(f)
            header = next(reader)
            return self.create(name, header, ([_cell(c) for c in row] for row in reader))

    def load_dir(self, path):
        for p in sorted(Path(path).glob("*.csv")):
            self.load_csv(p.stem, p)


def _cell(text):
    if text == "":
        return None
    for conv in (int, float):
        try:
            return conv(text)
        except ValueError:
            pass
    return text
