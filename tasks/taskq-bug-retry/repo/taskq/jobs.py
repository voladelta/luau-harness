from dataclasses import dataclass


@dataclass
class Job:
    id: str
    name: str
    payload: dict
    priority: int = 0
    attempts: int = 0
    max_attempts: int = 3
    run_at: float = 0.0
    status: str = "queued"  # queued | running | done | dead
    error: str | None = None
