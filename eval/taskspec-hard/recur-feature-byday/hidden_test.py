import unittest
from datetime import date

from recur.calendar import Calendar, Event
from recur.describe import describe
from recur.expand import between, first_n
from recur.rule import format_rule, parse


def d(s):
    return date.fromisoformat(s)


def ds(*xs):
    return [d(x) for x in xs]


def first(rule, start, n):
    return first_n(parse(rule), d(start), n)


class T(unittest.TestCase):
    def test_monthly_ordinals(self):
        self.assertEqual(first("FREQ=MONTHLY;BYDAY=-1FR", "2025-01-01", 4), ds("2025-01-31", "2025-02-28", "2025-03-28", "2025-04-25"))
        self.assertEqual(first("FREQ=MONTHLY;BYDAY=2TU,+4TU", "2025-01-01", 4), ds("2025-01-14", "2025-01-28", "2025-02-11", "2025-02-25"))
        self.assertEqual(first("FREQ=MONTHLY;BYDAY=5MO", "2025-01-01", 3), ds("2025-03-31", "2025-06-30", "2025-09-29"))
        self.assertEqual(first("FREQ=MONTHLY;BYDAY=1MO,-1MO,WE", "2025-02-01", 6),
                         ds("2025-02-03", "2025-02-05", "2025-02-12", "2025-02-19", "2025-02-24", "2025-02-26"))
        self.assertEqual(first("FREQ=MONTHLY;INTERVAL=3;BYDAY=-2SA", "2025-01-20", 3), ds("2025-04-19", "2025-07-19", "2025-10-18"))

    def test_yearly_ordinals(self):
        self.assertEqual(first("FREQ=YEARLY;BYDAY=20MO", "2025-01-01", 2), ds("2025-05-19", "2026-05-18"))
        self.assertEqual(first("FREQ=YEARLY;BYDAY=-1SU", "2025-01-01", 2), ds("2025-12-28", "2026-12-27"))
        self.assertEqual(first("FREQ=YEARLY;BYMONTH=11;BYDAY=4TH", "2025-01-01", 3), ds("2025-11-27", "2026-11-26", "2027-11-25"))
        self.assertEqual(first("FREQ=YEARLY;BYMONTH=5,9;BYDAY=-1MO", "2025-06-01", 3), ds("2025-09-29", "2026-05-25", "2026-09-28"))
        self.assertEqual(first("FREQ=YEARLY;BYDAY=1MO,-1MO", "2025-03-01", 2), ds("2025-12-29", "2026-01-05"))
        self.assertEqual(len(between(parse("FREQ=YEARLY;BYDAY=FR"), d("2025-01-01"), d("2025-01-01"), d("2025-12-31"))), 52)
        self.assertEqual(first("FREQ=YEARLY;BYDAY=53TH", "2025-01-01", 1), ds("2026-12-31"))

    def test_negative_monthday(self):
        self.assertEqual(first("FREQ=MONTHLY;BYMONTHDAY=-1", "2024-01-15", 4), ds("2024-01-31", "2024-02-29", "2024-03-31", "2024-04-30"))
        self.assertEqual(first("FREQ=MONTHLY;BYMONTHDAY=-3,1", "2025-02-01", 4), ds("2025-02-01", "2025-02-26", "2025-03-01", "2025-03-29"))
        self.assertEqual(first("FREQ=MONTHLY;BYMONTHDAY=-31", "2025-01-01", 3), ds("2025-01-01", "2025-03-01", "2025-05-01"))
        self.assertEqual(first("FREQ=YEARLY;BYMONTH=2;BYMONTHDAY=-1", "2023-01-01", 2), ds("2023-02-28", "2024-02-29"))
        self.assertEqual(first("FREQ=DAILY;BYMONTHDAY=-1", "2025-01-02", 2), ds("2025-01-31", "2025-02-28"))
        self.assertEqual(first("FREQ=MONTHLY;BYMONTHDAY=-7,-6,-5,-4,-3,-2,-1;BYDAY=FR", "2025-01-01", 2), ds("2025-01-31", "2025-02-28"))

    def test_bymonthday_and_byday(self):
        self.assertEqual(first("FREQ=MONTHLY;BYDAY=FR;BYMONTHDAY=13", "2025-01-01", 3), ds("2025-06-13", "2026-02-13", "2026-03-13"))
        self.assertEqual(first("FREQ=MONTHLY;BYDAY=1SU;BYMONTHDAY=1,2,3", "2025-01-01", 3), ds("2025-02-02", "2025-03-02", "2025-06-01"))

    def test_bysetpos(self):
        self.assertEqual(first("FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=-1", "2025-01-01", 4),
                         ds("2025-01-31", "2025-02-28", "2025-03-31", "2025-04-30"))
        self.assertEqual(first("FREQ=MONTHLY;BYDAY=SA,SU;BYSETPOS=1,-1", "2025-03-01", 4),
                         ds("2025-03-01", "2025-03-30", "2025-04-05", "2025-04-27"))
        self.assertEqual(first("FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=1", "2025-01-15", 2), ds("2025-02-03", "2025-03-03"))
        self.assertEqual(first("FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=3,-1,3;COUNT=3", "2025-01-01", 9),
                         ds("2025-01-03", "2025-01-31", "2025-02-05"))
        self.assertEqual(first("FREQ=MONTHLY;BYMONTHDAY=1,15,-1;BYSETPOS=2", "2025-01-01", 2), ds("2025-01-15", "2025-02-15"))
        self.assertEqual(first("FREQ=YEARLY;BYMONTH=1,7;BYDAY=MO;BYSETPOS=-1", "2025-01-01", 2), ds("2025-07-28", "2026-07-27"))
        self.assertEqual(first("FREQ=MONTHLY;BYDAY=1MO,2MO;BYSETPOS=3", "2025-01-01", 2), [])
        self.assertEqual(first("FREQ=MONTHLY;UNTIL=20250415;BYDAY=TU,TH;BYSETPOS=-2", "2025-01-01", 9),
                         ds("2025-01-28", "2025-02-25", "2025-03-25"))

    def test_validation(self):
        for bad in ("FREQ=WEEKLY;BYDAY=1MO", "FREQ=DAILY;BYDAY=-1FR", "FREQ=MONTHLY;BYDAY=0MO", "FREQ=MONTHLY;BYDAY=54MO",
                    "FREQ=MONTHLY;BYDAY=-54MO", "FREQ=MONTHLY;BYDAY=XMO", "FREQ=MONTHLY;BYDAY=1XX", "FREQ=MONTHLY;BYMONTHDAY=0",
                    "FREQ=MONTHLY;BYMONTHDAY=-32", "FREQ=MONTHLY;BYDAY=MO;BYSETPOS=0", "FREQ=MONTHLY;BYDAY=MO;BYSETPOS=367",
                    "FREQ=WEEKLY;BYDAY=MO;BYSETPOS=1", "FREQ=MONTHLY;BYSETPOS=1"):
            with self.subTest(rule=bad), self.assertRaises(ValueError):
                parse(bad)
        parse("FREQ=YEARLY;BYDAY=53MO,-53SU;BYSETPOS=366,-366")
        parse("FREQ=WEEKLY;BYMONTHDAY=-1")

    def test_format(self):
        r = parse("FREQ=MONTHLY;BYSETPOS=1,-1;BYDAY=+2TU,-1FR,WE;BYMONTHDAY=-1,5;COUNT=3")
        self.assertEqual(format_rule(r), "FREQ=MONTHLY;BYMONTHDAY=-1,5;BYDAY=2TU,-1FR,WE;BYSETPOS=1,-1;COUNT=3")
        self.assertEqual(format_rule(parse(format_rule(r))), format_rule(r))
        self.assertEqual(parse(format_rule(r)), r)
        self.assertEqual(format_rule(parse("FREQ=WEEKLY;BYDAY=FR,MO")), "FREQ=WEEKLY;BYDAY=FR,MO")

    def test_describe(self):
        self.assertEqual(describe(parse("FREQ=MONTHLY;BYDAY=-1FR")), "monthly on the last Friday")
        self.assertEqual(describe(parse("FREQ=MONTHLY;BYDAY=4TU,2TU;COUNT=6")), "monthly on the fourth Tuesday and the second Tuesday, 6 times")
        self.assertEqual(describe(parse("FREQ=YEARLY;BYMONTH=11;BYDAY=4TH")), "yearly in November on the fourth Thursday")
        self.assertEqual(describe(parse("FREQ=YEARLY;BYDAY=20MO,-21SU,1TU")), "yearly on the 20th Monday, the first Tuesday and the 21st to last Sunday")
        self.assertEqual(describe(parse("FREQ=YEARLY;BYDAY=11WE,12WE,13WE,22WE,23WE,5WE")),
                         "yearly on the 11th Wednesday, the 12th Wednesday, the 13th Wednesday, the 22nd Wednesday, the 23rd Wednesday and the fifth Wednesday")
        self.assertEqual(describe(parse("FREQ=MONTHLY;INTERVAL=2;BYDAY=FR,-2WE,3MO")), "every 2 months on the third Monday, the second to last Wednesday and Friday")
        self.assertEqual(describe(parse("FREQ=YEARLY;BYDAY=-6SA")), "yearly on the 6th to last Saturday")
        self.assertEqual(describe(parse("FREQ=WEEKLY;BYDAY=WE,MO")), "weekly on Monday and Wednesday")
        for rule in ("FREQ=MONTHLY;BYMONTHDAY=-1,-2", "FREQ=MONTHLY;BYDAY=MO,FR;BYSETPOS=-1", "FREQ=MONTHLY;BYDAY=-1FR;BYMONTHDAY=-7,-6,-5"):
            self.assertIsInstance(describe(parse(rule)), str)

    def test_calendar(self):
        c = Calendar()
        c.add(Event.recurring("payday", d("2025-01-01"), "FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=-1", exdates=[d("2025-02-28")]))
        c.add(Event.recurring("review", d("2025-01-01"), "FREQ=MONTHLY;BYDAY=-1FR"))
        self.assertEqual(c.between(d("2025-01-30"), d("2025-03-01")), [(d("2025-01-31"), "payday"), (d("2025-01-31"), "review"), (d("2025-02-28"), "review")])
        self.assertEqual(c.get("payday").next_after(d("2025-01-31")), d("2025-03-31"))
