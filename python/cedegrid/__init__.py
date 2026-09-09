"""CedeGrid 0.2: mTLS operator clients and native supervised workers."""
from .client import Client, RemoteError, command_task
from .worker import WorkerContext, DrainRequested, canonical_json, sha256_file
from .process import ManagedChild, SpawnUncertain, spawn_managed

__all__ = ["Client", "RemoteError", "command_task", "WorkerContext", "DrainRequested", "canonical_json", "sha256_file", "ManagedChild", "SpawnUncertain", "spawn_managed"]

from .codec import parse_json, stringify_json
from .config import load_client_config
from .errors import CedeGridError, ValidationError, ConfigError, DeadlineExceeded, PublicationUncertain, DownloadUncertain, is_cedegrid_error
__all__ += ["parse_json", "stringify_json", "load_client_config", "CedeGridError", "ValidationError", "ConfigError", "DeadlineExceeded", "PublicationUncertain", "DownloadUncertain", "is_cedegrid_error"]
