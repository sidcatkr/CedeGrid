import { parse, stringify } from 'lossless-json';
import { ValidationError } from './errors';
export type JsonValue = null | boolean | string | number | bigint | JsonValue[] | {
    [key: string]: JsonValue;
};
export type IntegerInput = bigint | number;
export const U64_MAX = (1n << 64n) - 1n;
export const I64_MAX = (1n << 63n) - 1n;
export const I64_MIN = -(1n << 63n);
export function integer(value: unknown, name = 'integer', minimum = 0n, maximum = U64_MAX): bigint {
    if (typeof value === 'number' && Number.isSafeInteger(value))
        value = BigInt(value);
    if (typeof value !== 'bigint' || value < minimum || value > maximum)
        throw new ValidationError(`${name} must be an exact integer in ${minimum}..${maximum}`);
    return value;
}
function parseNumber(token: string): bigint | number {
    if (token === '-0')
        return -0;
    if (/^-?\d+$/.test(token))
        return integer(BigInt(token), 'JSON integer', I64_MIN, U64_MAX);
    const value = Number(token), significand = token.split(/[eE]/)[0];
    if (!Number.isFinite(value) || (value === 0 && /[1-9]/.test(significand)))
        throw new ValidationError('JSON float is nonfinite, overflowing or underflowing');
    return value;
}
// Prefix object keys before passing them to the dependency: its ordinary
// assignment would otherwise treat __proto__ as a prototype setter. Numeric
// tokens remain untouched and go through lossless-json's numeric hook.
function protectKeys(text: string): string {
    let result = '', cursor = 0;
    for (let i = 0; i < text.length; i++) {
        if (text[i] !== '"')
            continue;
        const start = i++;
        for (; i < text.length; i++) {
            if (text[i] === '\\') {
                i++;
                continue;
            }
            if (text[i] === '"')
                break;
        }
        const end = i + 1;
        let next = end;
        while (/\s/.test(text[next] ?? '') && next < text.length)
            next++;
        if (text[next] === ':') {
            const key: unknown = JSON.parse(text.slice(start, end));
            result += text.slice(cursor, start) + JSON.stringify('$cedegrid$' + key);
            cursor = end;
        }
    }
    return result + text.slice(cursor);
}
function restoreKeys(value: any): any {
    if (Array.isArray(value))
        return value.map(restoreKeys);
    if (value && typeof value === 'object') {
        const result: Record<string, unknown> = {};
        for (const [key, item] of Object.entries(value))
            Object.defineProperty(result, key.slice(10), { value: restoreKeys(item), enumerable: true, configurable: true, writable: true });
        return result;
    }
    return value;
}
export function parseJson(text: string): JsonValue {
    try {
        const value = restoreKeys(parse(protectKeys(text), undefined, { parseNumber }));
        validateJson(value);
        return value;
    }
    catch (cause) {
        if (cause instanceof ValidationError)
            throw cause;
        throw new ValidationError('invalid JSON', { cause });
    }
}
export function validateJson(value: unknown, active = new Set<object>()): asserts value is JsonValue {
    if (value === null || typeof value === 'boolean')
        return;
    if (typeof value === 'string') {
        if (!value.isWellFormed())
            throw new ValidationError('unpaired Unicode surrogate');
        return;
    }
    if (typeof value === 'bigint') {
        integer(value, 'JSON integer', I64_MIN, U64_MAX);
        return;
    }
    if (typeof value === 'number') {
        if (!Number.isFinite(value))
            throw new ValidationError('JSON floats must be finite');
        return;
    }
    if (typeof value !== 'object' || (!Array.isArray(value) && Object.getPrototypeOf(value) !== Object.prototype && Object.getPrototypeOf(value) !== null))
        throw new ValidationError('JSON requires plain objects and arrays');
    if (active.has(value))
        throw new ValidationError('cyclic JSON value');
    active.add(value);
    for (const [key, item] of Object.entries(value)) {
        validateJson(key, active);
        validateJson(item, active);
    }
    active.delete(value);
}
export function stringifyJson(value: unknown): string {
    validateJson(value);
    return stringify(value, undefined, undefined, [{ test: (v: unknown) => typeof v === 'number', stringify: (v: unknown) => {
                if (Object.is(v, -0))
                    return '-0.0';
                const text = String(v);
                return /[.eE]/.test(text) ? text : `${text}.0`;
            } }])!;
}
/** Normalize declared wire integers without changing arbitrary metadata values. */
export function protocolValue(value: unknown, field = ''): unknown {
    if (field === 'metadata' || field === 'result') {
        validateJson(value);
        return value;
    }
    if (value === null || value === undefined)
        return value;
    if (field === 'env') {
        if (typeof value !== 'object' || Array.isArray(value) || Object.values(value).some(item => typeof item !== 'string'))
            throw new ValidationError('env must be a string mapping');
        return value;
    }
    if (field === 'gpu_memory_mib') {
        if (typeof value !== 'object' || Array.isArray(value))
            throw new ValidationError('GPU memory must be a mapping');
        return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, integer(item, 'GPU memory')]));
    }
    if (['generation', 'coordinator_epoch', 'lease_sequence', 'checkpoint_sequence', 'sequence'].includes(field))
        return integer(value, field, 0n, I64_MAX);
    if (['version', 'schema_version', 'max_attempts', 'launch_slots', 'max_workers', 'min_workers', 'pid', 'uid', 'retry_limit'].includes(field))
        return integer(value, field, 0n, 4294967295n);
    if (field === 'managed_child_limit')
        return integer(value, field, 0n, 8n);
    if (field === 'limit')
        return integer(value, field, 1n, 1000n);
    if (field === 'priority' || field === 'exit_code')
        return integer(value, field, -2147483648n, 2147483647n);
    if (['cpu_millicores', 'ram_mib', 'size', 'offset', 'max_bytes', 'observed_at_unix_ms', 'start_ticks', 'start_time', 'query_cursor_us', 'samples_returned', 'sample_max_age_ms', 'total_memory_mib', 'cpu_capacity_millicores', 'total_ram_mib', 'lease_ms', 'valid_for_ms', 'created_at_unix_ms', 'max_artifact_bytes', 'artifact_quota_bytes'].includes(field))
        return integer(value, field);
    if (Array.isArray(value))
        return value.map(item => protocolValue(item));
    if (typeof value === 'object')
        return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, protocolValue(item, key)]));
    return value;
}
