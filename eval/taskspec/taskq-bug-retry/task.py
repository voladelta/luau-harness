DOMAIN = "taskq"
MUTATIONS = [("taskq/retry.py", "return job.attempts < job.max_attempts", "return job.attempts <= job.max_attempts")]
