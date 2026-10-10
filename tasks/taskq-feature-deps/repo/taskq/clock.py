import time


class Clock:
    def now(self):
        return time.time()


class FakeClock(Clock):
    def __init__(self, t=0.0):
        self.t = t

    def now(self):
        return self.t

    def advance(self, seconds):
        self.t += seconds
