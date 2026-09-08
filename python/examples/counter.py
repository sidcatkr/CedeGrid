"""Ordinary cooperative command: bounded counting with durable continuation."""
import argparse
import time
from cedegrid import WorkerContext

p = argparse.ArgumentParser()
p.add_argument("--steps", type=int, default=20)
p.add_argument("--delay", type=float, default=0.1)
a = p.parse_args()
context = WorkerContext.from_env()
start = (context.resume or {}).get("metadata", {}).get("completed_steps", 0)
for step in range(start, a.steps):
    if context.draining():
        context.checkpoint({"completed_steps": step})
        raise SystemExit(75)
    time.sleep(a.delay)
context.complete({"completed_steps": a.steps})
