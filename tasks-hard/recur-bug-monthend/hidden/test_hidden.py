import unittest
from datetime import date

from recur.calendar import Calendar, Event
from recur.dates import shift_months
from recur.expand import between, first_n
from recur.rule import parse


def d(s):
    return date.fromisoformat(s)


def ds(*xs):
    return [d(x) for x in xs]


class T(unittest.TestCase):
    def test_monthly_on_31st(self):
        self.assertEqual(first_n(parse("FREQ=MONTHLY"), d("2025-01-31"), 7),
                         ds("2025-01-31", "2025-03-31", "2025-05-31", "2025-07-31", "2025-08-31", "2025-10-31", "2025-12-31"))

    def test_count_counts_real_dates(self):
        self.assertEqual(first_n(parse("FREQ=MONTHLY;COUNT=4"), d("2025-01-31"), 10),
                         ds("2025-01-31", "2025-03-31", "2025-05-31", "2025-07-31"))

    def test_bymonthday(self):
        self.assertEqual(first_n(parse("FREQ=MONTHLY;BYMONTHDAY=30"), d("2025-01-01"), 3), ds("2025-01-30", "2025-03-30", "2025-04-30"))
        self.assertEqual(first_n(parse("FREQ=MONTHLY;BYMONTHDAY=31,29"), d("2025-01-01"), 6),
                         ds("2025-01-29", "2025-01-31", "2025-03-29", "2025-03-31", "2025-04-29", "2025-05-29"))
        self.assertEqual(first_n(parse("FREQ=MONTHLY;BYMONTHDAY=29"), d("2024-01-29"), 3), ds("2024-01-29", "2024-02-29", "2024-03-29"))

    def test_yearly_leap_day(self):
        self.assertEqual(first_n(parse("FREQ=YEARLY"), d("2024-02-29"), 3), ds("2024-02-29", "2028-02-29", "2032-02-29"))
        self.assertEqual(first_n(parse("FREQ=YEARLY;BYMONTH=1,2,3"), d("2023-01-31"), 4),
                         ds("2023-01-31", "2023-03-31", "2024-01-31", "2024-03-31"))

    def test_interval_and_until(self):
        self.assertEqual(first_n(parse("FREQ=MONTHLY;INTERVAL=2"), d("2025-01-31"), 5),
                         ds("2025-01-31", "2025-03-31", "2025-05-31", "2025-07-31", "2026-01-31"))
        self.assertEqual(first_n(parse("FREQ=MONTHLY;UNTIL=20250531"), d("2025-01-30"), 10),
                         ds("2025-01-30", "2025-03-30", "2025-04-30", "2025-05-30"))

    def test_calendar(self):
        c = Calendar()
        c.add(Event.recurring("rent", d("2025-01-31"), "FREQ=MONTHLY"))
        self.assertEqual(c.between(d("2025-02-01"), d("2025-02-28")), [])
        self.assertEqual(c.get("rent").next_after(d("2025-01-31")), d("2025-03-31"))
        self.assertEqual(c.between(d("2025-04-01"), d("2025-05-31")), [(d("2025-05-31"), "rent")])

    def test_unchanged(self):
        self.assertEqual(first_n(parse("FREQ=MONTHLY"), d("2025-01-15"), 3), ds("2025-01-15", "2025-02-15", "2025-03-15"))
        self.assertEqual(shift_months(d("2024-01-31"), 1), d("2024-02-29"))
        self.assertEqual(shift_months(d("2025-03-31"), -1), d("2025-02-28"))
        e = Event.recurring("x", d("2025-01-31"), "FREQ=MONTHLY")
        e.postpone(1)
        self.assertEqual(e.start, d("2025-02-28"))
        self.assertEqual(first_n(e.rule, e.start, 2), ds("2025-02-28", "2025-03-28"))
        got = between(parse("FREQ=MONTHLY;BYDAY=FR"), d("2025-02-01"), d("2025-02-01"), d("2025-02-28"))
        self.assertEqual([x.day for x in got], [7, 14, 21, 28])
