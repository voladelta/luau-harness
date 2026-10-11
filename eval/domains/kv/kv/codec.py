"""Line framing shared by the log and segment files: b"<crc32 as 8 hex digits> <json>\\n" per record."""
import json
import zlib


class CorruptError(Exception):
    pass


def encode(record):
    body = json.dumps(record, separators=(",", ":"), sort_keys=True).encode()
    return b"%08x " % zlib.crc32(body) + body + b"\n"


def decode_line(line):
    """One framed line without its newline -> record. Raises CorruptError."""
    if len(line) < 10 or line[8:9] != b" ":
        raise CorruptError("short or malformed line")
    body = line[9:]
    try:
        crc = int(line[:8], 16)
    except ValueError:
        raise CorruptError("bad checksum field") from None
    if zlib.crc32(body) != crc:
        raise CorruptError("checksum mismatch")
    try:
        return json.loads(body)
    except ValueError:
        raise CorruptError("bad json") from None


def decode_all(data):
    """-> (records, valid_length): records up to the first torn or corrupt line, and how many bytes they span.
    A last line without its newline counts as torn."""
    records, pos = [], 0
    while pos < len(data):
        end = data.find(b"\n", pos)
        if end < 0:
            break
        try:
            records.append(decode_line(data[pos:end]))
        except CorruptError:
            break
        pos = end + 1
    return records, pos
