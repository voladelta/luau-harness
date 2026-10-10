from datetime import datetime, timezone


def parse_ts(s):
    """Parse an ISO-8601 UTC timestamp like 2026-01-05T10:00:03Z."""
    return datetime.fromisoformat(s.replace("Z", "+00:00")).astimezone(timezone.utc)


def fmt_ts(dt):
    return dt.strftime("%Y-%m-%dT%H:%M:%SZ")


def bucket(dt, seconds):
    """Floor `dt` to a multiple of `seconds` since the epoch."""
    t = int(dt.timestamp())
    return datetime.fromtimestamp(t - t % seconds, tz=timezone.utc)
