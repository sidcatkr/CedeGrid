"""Versioned TOML client configuration; paths use the real config directory."""
from pathlib import Path
import math
import urllib.parse
try:
    import tomllib
except ImportError:
    import tomli as tomllib
from .codec import I64_MAX, I64_MIN, integer
from .errors import ConfigError, ValidationError

def endpoint_origin(endpoint):
    if type(endpoint) is not str or any(c in endpoint for c in "?#@\\") or any(ord(c) <= 32 or ord(c) == 127 for c in endpoint):
        raise ConfigError("endpoint must be an HTTPS origin without credentials, query or fragment")
    try:
        parsed = urllib.parse.urlsplit(endpoint)
        port = parsed.port
        if parsed.scheme != "https" or not parsed.hostname or parsed.path not in ("", "/") or parsed.netloc.endswith(":") or (port is not None and not 1 <= port <= 65535):
            raise ValueError("not an HTTPS origin")
    except ValueError as error:
        raise ConfigError("invalid HTTPS endpoint origin", cause=error) from error
    return endpoint.rstrip("/")

def positive_number(value, name, allow_zero=False):
    if type(value) not in (int, float) or not math.isfinite(value) or (value < 0 if allow_zero else value <= 0):
        raise ConfigError(f"{name} must be finite and positive")
    return float(value)

def load_client_config(path):
    path = Path(path).resolve(strict=True)
    try:
        with path.open("rb") as stream:
            value = tomllib.load(stream)
    except (ValueError, TypeError) as error:
        raise ConfigError("invalid TOML client configuration", cause=error) from error
    def validate_scalars(obj):
        if type(obj) is int and not I64_MIN <= obj <= I64_MAX:
            raise ConfigError("TOML integers must fit signed 64-bit")
        if type(obj) is dict:
            for item in obj.values(): validate_scalars(item)
        elif type(obj) is list:
            for item in obj: validate_scalars(item)
    validate_scalars(value)
    if set(value) - {"config_version", "endpoint", "tls", "timeout_seconds", "max_transfer_bytes_per_second"}:
        raise ConfigError("unknown client configuration field")
    if type(value.get("config_version")) is not int or value["config_version"] != 1:
        raise ConfigError("config_version = 1 is required")
    tls = value.get("tls")
    if type(tls) is not dict or set(tls) != {"ca_cert", "certificate", "private_key"}:
        raise ConfigError("tls requires exactly ca_cert, certificate and private_key")
    try:
        rate = integer(value.get("max_transfer_bytes_per_second", 10 * 1024 * 1024), "max_transfer_bytes_per_second", 0, I64_MAX)
    except ValidationError as error:
        raise ConfigError(str(error), cause=error) from error
    result = {"endpoint": endpoint_origin(value.get("endpoint")),
              "timeout": positive_number(value.get("timeout_seconds", 15), "timeout_seconds"),
              "max_transfer_bytes_per_second": rate}
    for source, target in (("ca_cert", "ca"), ("certificate", "certificate"), ("private_key", "private_key")):
        item = tls[source]
        if type(item) is not str or not item or "\x00" in item:
            raise ConfigError(f"tls.{source} must be a nonempty path")
        result[target] = Path(__import__("os").path.normpath(path.parent / item))
    return result
