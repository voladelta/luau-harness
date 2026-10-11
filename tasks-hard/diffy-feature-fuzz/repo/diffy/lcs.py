"""Line diff based on the longest common subsequence."""


def diff(a, b):
    """-> list of (tag, line) with tag " " (unchanged), "-" (only in a) or "+" (only in b).

    Within a changed region, deletions come before insertions.
    """
    n, m = len(a), len(b)
    pre = 0
    while pre < n and pre < m and a[pre] == b[pre]:
        pre += 1
    suf = 0
    while suf < n - pre and suf < m - pre and a[n - 1 - suf] == b[m - 1 - suf]:
        suf += 1
    xs, ys = a[pre:n - suf], b[pre:m - suf]
    # lcs[i][j] = LCS length of xs[i:] and ys[j:]
    lcs = [[0] * (len(ys) + 1) for _ in range(len(xs) + 1)]
    for i in range(len(xs) - 1, -1, -1):
        row, below = lcs[i], lcs[i + 1]
        for j in range(len(ys) - 1, -1, -1):
            row[j] = below[j + 1] + 1 if xs[i] == ys[j] else max(below[j], row[j + 1])
    ops = [(" ", x) for x in a[:pre]]
    i = j = 0
    while i < len(xs) and j < len(ys):
        if xs[i] == ys[j]:
            ops.append((" ", xs[i]))
            i += 1
            j += 1
        elif lcs[i + 1][j] >= lcs[i][j + 1]:
            ops.append(("-", xs[i]))
            i += 1
        else:
            ops.append(("+", ys[j]))
            j += 1
    ops += [("-", x) for x in xs[i:]] + [("+", y) for y in ys[j:]]
    ops += [(" ", x) for x in a[n - suf:]]
    return ops
