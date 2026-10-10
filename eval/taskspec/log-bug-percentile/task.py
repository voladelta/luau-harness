DOMAIN = "logkit"
MUTATIONS = [("logkit/stats.py", "    rank = max(1, math.ceil(p / 100 * len(s)))\n    return s[rank - 1]\n", "    return s[min(len(s) - 1, int(p / 100 * len(s)))]\n")]
