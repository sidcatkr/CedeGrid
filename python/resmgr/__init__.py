"""ResourceManager v1: transport-independent cooperative worker contracts."""
from .client import Client, RemoteError, command_task
from .worker import WorkerContext, DrainRequested, canonical_json, sha256_file
from .process import ManagedChild, SpawnUncertain, spawn_managed

__all__ = ["Client", "RemoteError", "command_task", "WorkerContext", "DrainRequested", "canonical_json", "sha256_file", "ManagedChild", "SpawnUncertain", "spawn_managed"]
