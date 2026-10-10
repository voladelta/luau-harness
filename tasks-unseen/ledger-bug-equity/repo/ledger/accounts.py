TYPES = ("asset", "liability", "equity", "income", "expense")


class Chart:
    def __init__(self):
        self.accounts = {}

    def add(self, name, type_):
        if type_ not in TYPES:
            raise ValueError(f"bad account type: {type_}")
        if name in self.accounts:
            raise ValueError(f"duplicate account: {name}")
        self.accounts[name] = type_

    def type_of(self, name):
        if name not in self.accounts:
            raise ValueError(f"unknown account: {name}")
        return self.accounts[name]
