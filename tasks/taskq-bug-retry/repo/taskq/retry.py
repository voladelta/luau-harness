def backoff(attempt, base=2.0, cap=60.0):
    """Delay in seconds before retry number `attempt` (1 = first retry): base * 2**(attempt-1), capped."""
    return min(cap, base * 2 ** (attempt - 1))


def should_retry(job):
    """A job may run at most `max_attempts` times in total."""
    return job.attempts <= job.max_attempts
