"""Calendar helpers. Weekdays are ints, 0 = Monday ... 6 = Sunday, as in datetime.date.weekday()."""
import calendar
from datetime import date, timedelta

WEEKDAY_CODES = ["MO", "TU", "WE", "TH", "FR", "SA", "SU"]
WEEKDAY_NAMES = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"]
MONTH_NAMES = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October",
               "November", "December"]


def days_in_month(year, month):
    return calendar.monthrange(year, month)[1]


def make_date(year, month, day):
    """date(year, month, day), or None when the month has no such day (e.g. February 30)."""
    if 1 <= day <= days_in_month(year, month):
        return date(year, month, day)
    return None


def add_months(year, month, n):
    """(year, month) n months after the given month; n may be negative."""
    k = year * 12 + (month - 1) + n
    return k // 12, k % 12 + 1


def shift_months(d, n):
    """The same day n months later, clamped to the end of a shorter month (Jan 31 + 1 month = Feb 28/29)."""
    y, m = add_months(d.year, d.month, n)
    return date(y, m, min(d.day, days_in_month(y, m)))


def week_start(d):
    """The Monday of d's week."""
    return d - timedelta(days=d.weekday())


def month_days(year, month):
    return [date(year, month, i) for i in range(1, days_in_month(year, month) + 1)]


def parse_date(text):
    """YYYYMMDD or YYYY-MM-DD."""
    t = text.replace("-", "")
    if len(t) != 8 or not t.isdigit():
        raise ValueError(f"bad date {text!r}")
    return date(int(t[:4]), int(t[4:6]), int(t[6:]))
