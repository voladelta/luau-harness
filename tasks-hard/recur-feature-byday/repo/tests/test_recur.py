import unittest
from datetime import date

from recur.calendar import Calendar, Event
from recur.describe import describe
from recur.expand import between, first_n
from recur.rule import format_rule, parse


def d(s):
    return date.fromisoformat(s)


class RecurTests(unittest.TestCase):
    def test_parse_roundtrip(self):
        r = parse("freq=weekly;byday=we,mo;interval=2;count=4")
        self.assertEqual(format_rule(r), "FREQ=WEEKLY;INTERVAL=2;BYDAY=WE,MO;COUNT=4")

    def test_parse_errors(self):
        for bad in ("BYDAY=MO", "FREQ=HOURLY", "FREQ=DAILY;COUNT=2;UNTIL=20250101", "FREQ=DAILY;BYMONTHDAY=32"):
            with self.assertRaises(ValueError):
                parse(bad)

    def test_daily(self):
        self.assertEqual(first_n(parse("FREQ=DAILY;INTERVAL=3"), d("2025-01-30"), 3),
                         [d("2025-01-30"), d("2025-02-02"), d("2025-02-05")])

    def test_weekly(self):
        self.assertEqual(first_n(parse("FREQ=WEEKLY;BYDAY=MO,FR"), d("2025-03-05"), 4),
                         [d("2025-03-07"), d("2025-03-10"), d("2025-03-14"), d("2025-03-17")])

    def test_monthly_bymonthday(self):
        self.assertEqual(first_n(parse("FREQ=MONTHLY;BYMONTHDAY=1,15"), d("2025-01-10"), 3),
                         [d("2025-01-15"), d("2025-02-01"), d("2025-02-15")])

    def test_monthly_weekday(self):
        got = between(parse("FREQ=MONTHLY;BYDAY=TU"), d("2025-04-01"), d("2025-04-01"), d("2025-04-30"))
        self.assertEqual([x.day for x in got], [1, 8, 15, 22, 29])

    def test_yearly(self):
        self.assertEqual(first_n(parse("FREQ=YEARLY;BYMONTH=3,9"), d("2024-05-02"), 3),
                         [d("2024-09-02"), d("2025-03-02"), d("2025-09-02")])

    def test_count_includes_exdates(self):
        r = parse("FREQ=DAILY;COUNT=3")
        self.assertEqual(between(r, d("2025-01-01"), d("2025-01-01"), d("2025-12-31"), {d("2025-01-02")}),
                         [d("2025-01-01"), d("2025-01-03")])

    def test_until_inclusive(self):
        self.assertEqual(len(first_n(parse("FREQ=WEEKLY;UNTIL=20250115"), d("2025-01-01"), 10)), 3)

    def test_calendar(self):
        c = Calendar()
        c.add(Event.recurring("standup", d("2025-06-02"), "FREQ=WEEKLY;BYDAY=MO,WE"))
        c.add(Event("launch", d("2025-06-04")))
        self.assertEqual(c.between(d("2025-06-02"), d("2025-06-04")),
                         [(d("2025-06-02"), "standup"), (d("2025-06-04"), "standup"), (d("2025-06-04"), "launch")])
        self.assertEqual(c.get("standup").next_after(d("2025-06-04")), d("2025-06-09"))

    def test_describe(self):
        self.assertEqual(describe(parse("FREQ=WEEKLY;INTERVAL=2;BYDAY=WE,MO;COUNT=10")), "every 2 weeks on Monday and Wednesday, 10 times")
        self.assertEqual(describe(parse("FREQ=MONTHLY;BYMONTHDAY=13;BYDAY=FR")), "monthly on day 13 if a Friday")
        self.assertEqual(describe(parse("FREQ=YEARLY;BYMONTH=12;UNTIL=20301231")), "yearly in December, until 2030-12-31")


if __name__ == "__main__":
    unittest.main()
