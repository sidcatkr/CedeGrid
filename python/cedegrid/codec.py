"""Exact JSON integer tokens and finite binary64 metadata."""
import json
import math
from .errors import ValidationError
I64_MAX = (1 << 63) - 1
U64_MAX = (1 << 64) - 1
I64_MIN = -(1 << 63)

def integer(value, name="integer", minimum=0, maximum=U64_MAX):
    if type(value) is not int or not minimum <= value <= maximum:
        raise ValidationError(f"{name} must be an integer in {minimum}..{maximum}")
    return value

def _int(token):
    if token == "-0":
        return -0.0
    return integer(int(token), "JSON integer", I64_MIN, U64_MAX)

def _float(token):
    result = float(token)
    significand = token.lower().split("e")[0]
    if not math.isfinite(result) or (result == 0 and any(c in "123456789" for c in significand)):
        raise ValidationError("JSON float is nonfinite, overflowing or underflowing")
    return result

def _pairs(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValidationError(f"duplicate JSON key: {key}")
        result[key] = value
    return result

def parse_json(value):
    try:
        result = json.loads(value, parse_int=_int, parse_float=_float,
                          parse_constant=lambda token: (_ for _ in ()).throw(ValidationError(f"invalid JSON number: {token}")),
                          object_pairs_hook=_pairs)
        validate_json(result)
        return result
    except (ValueError, TypeError, UnicodeError) as error:
        if isinstance(error, ValidationError):
            raise
        raise ValidationError("invalid JSON", cause=error) from error

def validate_json(value, active=None):
    if value is None or type(value) in (str, bool):
        if type(value) is str:
            try:
                value.encode("utf-8")
            except UnicodeError as error:
                raise ValidationError("unpaired Unicode surrogate", cause=error) from error
        return
    if type(value) is int:
        integer(value, "JSON integer", I64_MIN, U64_MAX)
        return
    if type(value) is float:
        if not math.isfinite(value):
            raise ValidationError("JSON floats must be finite")
        return
    if type(value) not in (dict, list, tuple):
        raise ValidationError("JSON requires plain objects, arrays, strings, booleans and numbers")
    active = set() if active is None else active
    if id(value) in active:
        raise ValidationError("cyclic JSON value")
    active.add(id(value))
    try:
        if isinstance(value, dict):
            for key, item in value.items():
                if type(key) is not str:
                    raise ValidationError("JSON object keys must be strings")
                validate_json(key, active)
                validate_json(item, active)
        else:
            for item in value:
                validate_json(item, active)
    finally:
        active.remove(id(value))

def stringify_json(value, *, sort_keys=False):
    validate_json(value)
    return json.dumps(value, sort_keys=sort_keys, ensure_ascii=False, allow_nan=False, separators=(",", ":"))

def protocol_value(value, field=""):
    """Validate declared wire integer fields without changing arbitrary metadata."""
    if field in ("metadata", "result"):
        validate_json(value)
        return value
    if value is None: return value
    if field == "env":
        if type(value) is not dict or any(type(k) is not str or type(v) is not str for k, v in value.items()):
            raise ValidationError("env must be a string mapping")
        return value
    if field == "gpu_memory_mib":
        if type(value) is not dict: raise ValidationError("GPU memory must be a mapping")
        return {key: integer(item, "GPU memory") for key, item in value.items()}
    if field in ("generation", "coordinator_epoch", "lease_sequence", "checkpoint_sequence", "sequence"):
        return integer(value, field, 0, I64_MAX)
    if field in ("version", "schema_version", "max_attempts", "launch_slots", "max_workers", "min_workers", "pid", "uid", "retry_limit"):
        return integer(value, field, 0, 4294967295)
    if field == "managed_child_limit": return integer(value, field, 0, 8)
    if field == "limit": return integer(value, field, 1, 1000)
    if field in ("priority", "exit_code"): return integer(value, field, -2147483648, 2147483647)
    if field in ("cpu_millicores", "ram_mib", "size", "offset", "max_bytes", "observed_at_unix_ms", "start_ticks", "start_time", "query_cursor_us", "samples_returned", "sample_max_age_ms", "total_memory_mib", "cpu_capacity_millicores", "total_ram_mib", "lease_ms", "valid_for_ms", "created_at_unix_ms", "max_artifact_bytes", "artifact_quota_bytes"):
        return integer(value, field)
    if type(value) in (list, tuple): return [protocol_value(item) for item in value]
    if type(value) is dict: return {key: protocol_value(item, key) for key, item in value.items()}
    return value
