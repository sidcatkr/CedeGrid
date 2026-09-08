import { readFileSync, realpathSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { parse } from '@ltd/j-toml';
import { ConfigError } from './errors';
import { I64_MIN, I64_MAX, integer, IntegerInput } from './codec';
export interface ClientOptions {
    ca: string | Buffer;
    certificate: string | Buffer;
    privateKey: string | Buffer;
    timeoutSeconds?: number;
    maxTransferBytesPerSecond?: IntegerInput;
}
export interface ClientConfig extends ClientOptions {
    endpoint: string;
}
export function endpointOrigin(endpoint: string): string {
    if (typeof endpoint !== 'string' || /[?#@\\\x00-\x20\x7f]/.test(endpoint) || !/^https:\/\//.test(endpoint))
        throw new ConfigError('endpoint must be an HTTPS origin without credentials, query or fragment');
    try {
        const url = new URL(endpoint);
        const authority = endpoint.slice(8).split('/')[0];
        if (!url.hostname || url.pathname !== '/' || authority.endsWith(':') || (url.port && (!/^\d+$/.test(url.port) || Number(url.port) < 1 || Number(url.port) > 65535)))
            throw new Error('invalid origin');
        return endpoint.replace(/\/$/, '');
    }
    catch (cause) {
        throw new ConfigError('invalid HTTPS endpoint origin', { cause });
    }
}
export function positiveNumber(value: unknown, name: string, allowZero = false): number {
    if (typeof value !== 'number' || !Number.isFinite(value) || (allowZero ? value < 0 : value <= 0))
        throw new ConfigError(`${name} must be finite and ${allowZero ? 'nonnegative' : 'positive'}`);
    return value;
}
export function loadClientConfig(path: string): ClientConfig {
    const canonical = realpathSync(path), base = dirname(canonical);
    try {
        const value = parse(readFileSync(canonical, 'utf8'), 1.0, '\n', true) as Record<string, any>;
        function check(v: unknown): void {
            if (typeof v === 'bigint' && (v < I64_MIN || v > I64_MAX))
                throw new ConfigError('TOML integer exceeds signed 64-bit');
            if (v && typeof v === 'object')
                for (const x of Object.values(v))
                    check(x);
        }
        check(value);
        if (value.config_version !== 1n || Object.keys(value).some(k => !['config_version', 'endpoint', 'tls', 'timeout_seconds', 'max_transfer_bytes_per_second'].includes(k)))
            throw new ConfigError('config_version = 1 and known client fields are required');
        const tls = value.tls;
        if (!tls || typeof tls !== 'object' || Object.keys(tls).sort().join(',') !== 'ca_cert,certificate,private_key')
            throw new ConfigError('tls requires exactly ca_cert, certificate and private_key');
        const tlsPath = (key: string): string => { if (typeof tls[key] !== 'string' || !tls[key] || tls[key].includes('\0'))
            throw new ConfigError(`invalid tls.${key}`); return resolve(base, tls[key]); };
        const number = (key: string, fallback: number, allowZero = false): number => {
            let v = value[key] ?? fallback;
            if (typeof v === 'bigint') {
                if (v > BigInt(Number.MAX_SAFE_INTEGER) || v < BigInt(Number.MIN_SAFE_INTEGER))
                    throw new ConfigError(`${key} exceeds safe numeric range`);
                v = Number(v);
            }
            return positiveNumber(v, key, allowZero);
        };
        if (value.max_transfer_bytes_per_second !== undefined && typeof value.max_transfer_bytes_per_second !== 'bigint')
            throw new ConfigError('max_transfer_bytes_per_second must be a TOML integer');
        return { endpoint: endpointOrigin(value.endpoint), ca: tlsPath('ca_cert'), certificate: tlsPath('certificate'), privateKey: tlsPath('private_key'), timeoutSeconds: number('timeout_seconds', 15), maxTransferBytesPerSecond: integer(value.max_transfer_bytes_per_second ?? 10485760n, 'max_transfer_bytes_per_second', 0n, I64_MAX) };
    }
    catch (cause) {
        if (cause instanceof ConfigError)
            throw cause;
        throw new ConfigError('invalid TOML client configuration', { cause });
    }
}
