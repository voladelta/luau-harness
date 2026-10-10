class Worker:
    def __init__(self, queue, handlers):
        self.queue = queue
        self.handlers = handlers

    def run_once(self):
        job = self.queue.pop_ready()
        if job is None:
            return False
        handler = self.handlers.get(job.name)
        if handler is None:
            self.queue.fail(job, "no handler")
            return True
        try:
            handler(job.payload)
        except Exception as e:  # noqa: BLE001
            self.queue.fail(job, repr(e))
        else:
            self.queue.complete(job)
        return True

    def run_until_idle(self, max_steps=1000):
        n = 0
        while n < max_steps and self.run_once():
            n += 1
        return n
