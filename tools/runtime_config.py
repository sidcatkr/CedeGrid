"""Deterministic TOML runtime fixtures for the validation harnesses.

Application inputs, jobs, RPC payloads and evidence reports retain their JSON
formats. Cedegrid itself remains the authority for each runtime role's schema.
"""
import copy
import json
import math
from pathlib import Path

try:
    import tomllib
except ModuleNotFoundError:
    import tomli as tomllib

from validation_runtime import atomic_text


def _quoted(value):
    # JSON basic-string escapes are also TOML escapes. DEL must be escaped.
    return json.dumps(value, ensure_ascii=False).replace(chr(127), '\\u007f')


def _value(value):
    if isinstance(value, str):
        return _quoted(value)
    if isinstance(value, bool):
        return 'true' if value else 'false'
    if isinstance(value, int):
        if not -(2**63) <= value <= 2**63 - 1:
            raise ValueError('runtime TOML integers must fit signed 64 bits')
        return str(value)
    if isinstance(value, float):
        if not math.isfinite(value):
            raise ValueError('runtime TOML floats must be finite')
        return repr(value)
    if isinstance(value, list):
        return '[' + ', '.join(_value(item) for item in value) + ']'
    if isinstance(value, dict):
        return '{' + ', '.join(_quoted(key) + ' = ' + _value(item)
                              for key, item in value.items() if item is not None) + '}'
    raise TypeError('unsupported runtime TOML value: ' + type(value).__name__)


def runtime_toml(settings, kind):
    if kind not in ('node', 'coordinator', 'agent', 'client'):
        raise ValueError('unknown runtime config kind')
    settings = copy.deepcopy(settings)
    if kind == 'node':
        version = settings.pop('schema_version', 2)
        if type(version) is not int or version not in (1, 2):
            raise ValueError('unsupported internal node schema version')
        cgroup = settings.get('cgroup', {})
        if 'cpu_weight' in cgroup:
            weight = cgroup['cpu_weight']
            if weight is None:
                cgroup['cpu_weight'] = {'mode': 'off'}
            elif type(weight) is int:
                cgroup['cpu_weight'] = {'mode': 'set', 'value': weight}
    if 'config_version' in settings and (type(settings['config_version']) is not int or settings['config_version'] != 1):
        raise ValueError('runtime config_version must be integer 1')
    settings = {'config_version': 1, **settings}
    lines = []

    def table(values, path=()):
        if path:
            lines.append('[' + '.'.join(_quoted(key) for key in path) + ']')
        for key, value in values.items():
            if not isinstance(key, str):
                raise TypeError('runtime TOML keys must be strings')
            if value is not None and not isinstance(value, dict):
                lines.append(_quoted(key) + ' = ' + _value(value))
        for key, value in values.items():
            if isinstance(value, dict):
                lines.append('')
                table(value, (*path, key))

    table(settings)
    result = '\n'.join(lines) + '\n'
    tomllib.loads(result)
    return result


def atomic_runtime_config(path, settings, kind):
    atomic_text(path, runtime_toml(settings, kind))


def read_runtime_config(path):
    with Path(path).open('rb') as source:
        value = tomllib.load(source)
    if type(value.get('config_version')) is not int or value['config_version'] != 1:
        raise ValueError('runtime config_version must be integer 1')
    return value
