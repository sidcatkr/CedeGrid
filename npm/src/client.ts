import * as https from 'node:https';
import { readFileSync, createReadStream, promises as fs } from 'node:fs';
import { basename, dirname, join } from 'node:path';
import { createHash, randomUUID } from 'node:crypto';
import { performance } from 'node:perf_hooks';
import { setTimeout as delay } from 'node:timers/promises';
import { parseJson, stringifyJson, protocolValue, integer, IntegerInput, JsonValue, I64_MAX } from './codec';
import { ClientOptions, endpointOrigin, loadClientConfig, positiveNumber } from './config';
import { CedeGridError, DeadlineExceeded, DownloadUncertain, RemoteError, ValidationError } from './errors';
async function syncDirectory(path: string): Promise<void> {
    const directory = await fs.open(path, 'r');
    try { await directory.sync(); } finally { await directory.close(); }
}
async function downloadDirectory(path: string, strict: boolean): Promise<void> {
    const missing: string[] = [];
    let cursor = path;
    for (;;) {
        try {
            if (!(await fs.stat(cursor)).isDirectory())
                throw new ValidationError('download parent must be a directory');
            break;
        } catch (cause) {
            if ((cause as NodeJS.ErrnoException).code !== 'ENOENT') throw cause;
            missing.push(cursor);
            const parent = dirname(cursor);
            if (parent === cursor) throw cause;
            cursor = parent;
        }
    }
    for (const directory of missing.reverse()) {
        try { await fs.mkdir(directory); }
        catch (cause) { if ((cause as NodeJS.ErrnoException).code !== 'EEXIST') throw cause; }
        if (strict) { await syncDirectory(directory); await syncDirectory(dirname(directory)); }
    }
}
export type RpcReply = Record<string, any>;
export type StatusCollection = 'tasks' | 'jobs' | 'pools' | 'nodes' | 'allocations' | 'pool_nodes' | 'reported_allocations' | 'unrecognized_allocations';
export interface StatusFilters {
    jobId?: string;
    nodeId?: string;
    poolId?: string;
    limit?: IntegerInput;
    cursor?: string;
}
export interface Artifact {
    sha256: string;
    size: bigint;
}
export interface CommandOptions {
    cpuMillicores?: IntegerInput;
    ramMib?: IntegerInput;
    gpuVramMib?: Record<string, IntegerInput>;
    env?: Record<string, string>;
    replaySafe?: boolean;
    allocationClass?: 'guaranteed' | 'opportunistic';
    requiredControls?: string[];
    singleProcess?: boolean;
    noEscape?: boolean;
    managedChildLimit?: IntegerInput;
    inputArtifacts?: JsonValue[];
    maxAttempts?: IntegerInput;
}
export function textField(value: unknown, name: string, nonempty = false): asserts value is string { if (typeof value !== 'string' || value.includes('\0') || (nonempty && !value))
    throw new ValidationError(`${name} must be ${nonempty ? 'nonempty ' : ''}text without NUL`); }
export function argvField(argv: unknown): asserts argv is string[] { if (!Array.isArray(argv) || !argv.length)
    throw new ValidationError('argv must be an explicit nonempty string array'); argv.forEach(v => textField(v, 'argv')); textField(argv[0], 'executable', true); }
export function commandTask(taskId: string, argv: string[], cwd: string, options: CommandOptions = {}): Record<string, JsonValue> {
    textField(taskId, 'taskId', true);
    argvField(argv);
    textField(cwd, 'cwd', true);
    if (!/^(\/|[A-Za-z]:[\\/]|\\\\)/.test(cwd))
        throw new ValidationError('cwd must be absolute for the executing platform');
    for (const key of ['replaySafe', 'singleProcess', 'noEscape'] as const)
        if (options[key] !== undefined && typeof options[key] !== 'boolean')
            throw new ValidationError(`${key} must be boolean`);
    const childLimit = integer(options.managedChildLimit ?? 0n, 'managedChildLimit', 0n, 8n);
    if (childLimit && (options.singleProcess || !options.noEscape))
        throw new ValidationError('managed children require noEscape and singleProcess=false');
    const allocationClass = options.allocationClass ?? 'guaranteed';
    if (!['guaranteed', 'opportunistic'].includes(allocationClass))
        throw new ValidationError('invalid allocation class');
    const env = options.env ?? {};
    for (const [k, v] of Object.entries(env)) {
        textField(k, 'env key', true);
        textField(v, 'env value');
        if (k.includes('='))
            throw new ValidationError('invalid env key');
    }
    const controls = options.requiredControls ?? [];
    controls.forEach(v => textField(v, 'requiredControls'));
    const gpu: Record<string, bigint> = {};
    for (const [k, v] of Object.entries(options.gpuVramMib ?? {})) {
        textField(k, 'GPU key');
        Object.defineProperty(gpu, k, { value: integer(v, 'GPU memory'), enumerable: true });
    }
    const result: Record<string, JsonValue> = { task_id: taskId, assignment_id: '', argv: [...argv], cwd, env, resources: { cpu_millicores: integer(options.cpuMillicores ?? 1000n, 'cpuMillicores', 1n), ram_mib: integer(options.ramMib ?? 512n, 'ramMib', 1n), gpu_memory_mib: gpu }, replay_safe: options.replaySafe ?? false, class: allocationClass, no_escape: options.noEscape ?? false, single_process: options.singleProcess ?? false, required_controls: controls, allow_fallback: true };
    if (childLimit)
        result.managed_child_limit = childLimit;
    if (options.maxAttempts !== undefined)
        result.max_attempts = integer(options.maxAttempts, 'maxAttempts', 1n, 4294967295n);
    if (options.inputArtifacts)
        result.input_artifacts = options.inputArtifacts;
    return result;
}
export class Client {
    readonly url: URL;
    readonly timeoutSeconds: number;
    readonly maxTransferBytesPerSecond: bigint;
    private readonly tls: {
        ca: Buffer;
        cert: Buffer;
        key: Buffer;
    };
    private nextPace = 0;
    constructor(endpoint: string, options: ClientOptions) {
        this.url = new URL(endpointOrigin(endpoint) + '/v1/rpc');
        this.timeoutSeconds = positiveNumber(options.timeoutSeconds ?? 15, 'timeoutSeconds');
        this.maxTransferBytesPerSecond = integer(options.maxTransferBytesPerSecond ?? 10485760n, 'maxTransferBytesPerSecond');
        const load = (value: string | Buffer) => Buffer.isBuffer(value) ? value : readFileSync(value);
        this.tls = { ca: load(options.ca), cert: load(options.certificate), key: load(options.privateKey) };
    }
    static fromConfig(path: string): Client { const { endpoint, ...options } = loadClientConfig(path); return new Client(endpoint, options); }
    private async pace(bytes: number, deadline: number): Promise<void> {
        if (this.maxTransferBytesPerSecond === 0n || bytes <= 0)
            return;
        const now = performance.now();
        const next = Math.max(now, this.nextPace) + 1000 * bytes / (Number(this.maxTransferBytesPerSecond) * 0.9);
        if (next >= deadline)
            throw new DeadlineExceeded('RPC deadline expires while pacing');
        this.nextPace = next;
        await delay(next - now);
        if (performance.now() >= deadline)
            throw new DeadlineExceeded();
    }
    async request(op: string, payload: Record<string, unknown> = {}): Promise<RpcReply> {
        const deadline = performance.now() + this.timeoutSeconds * 1000;
        textField(op, 'op', true);
        if ('op' in payload)
            throw new ValidationError('payload cannot override operation');
        const body = Buffer.from(stringifyJson(protocolValue({ op, ...payload })));
        if (body.length > 3 * 1024 * 1024)
            throw new RemoteError('RPC request exceeds 3 MiB', 'ERR_CEDEGRID_REQUEST_TOO_LARGE');
        const reserve = op === 'read_artifact' ? 2 * Number(integer(payload.max_bytes ?? 0n, 'max_bytes', 0n, 1048576n)) + 512 : 0;
        await this.pace(body.length + reserve, deadline);
        const response = await new Promise<Buffer>((resolve, reject) => {
            let settled = false;
            const finish = (error?: Error, value?: Buffer) => { if (settled)
                return; settled = true; clearTimeout(timer); error ? reject(error) : resolve(value!); };
            const request = https.request(this.url, { method: 'POST', agent: false, ...this.tls, minVersion: 'TLSv1.2', rejectUnauthorized: true, headers: { 'content-type': 'application/json', 'content-length': body.length } }, res => {
                const limit = (op === 'status' || op === 'status_page' ? 8 : 3) * 1024 * 1024;
                const chunks: Buffer[] = [];
                let count = 0;
                res.on('data', (chunk: Buffer) => { count += chunk.length; if (count > limit) {
                    const error = new RemoteError('RPC response exceeds client bound', 'ERR_CEDEGRID_RESPONSE_TOO_LARGE');
                    finish(error);
                    request.destroy(error);
                    res.destroy(error);
                }
                else
                    chunks.push(chunk); });
                res.on('error', cause => finish(cause));
                res.on('end', () => { if (res.statusCode !== 200)
                    finish(new RemoteError(`RPC HTTP failure: ${res.statusCode}`, 'ERR_CEDEGRID_HTTP'));
                else
                    finish(undefined, Buffer.concat(chunks)); });
            });
            const timer = setTimeout(() => { const error = new DeadlineExceeded(); finish(error); request.destroy(error); }, Math.max(1, deadline - performance.now()));
            request.on('error', cause => finish(cause instanceof CedeGridError ? cause : new RemoteError('RPC transport failed', 'ERR_CEDEGRID_TRANSPORT', { cause })));
            request.end(body);
        }).catch(cause => { throw cause instanceof CedeGridError ? cause : new RemoteError('RPC transport failed', 'ERR_CEDEGRID_TRANSPORT', { cause }); });
        await this.pace(Math.max(0, response.length - reserve), deadline);
        if (performance.now() >= deadline)
            throw new DeadlineExceeded();
        const result = parseJson(new TextDecoder('utf-8', { fatal: true }).decode(response));
        if (performance.now() >= deadline)
            throw new DeadlineExceeded();
        if (!result || Array.isArray(result) || typeof result !== 'object')
            throw new RemoteError('invalid RPC response', 'ERR_CEDEGRID_PROTOCOL');
        if (result.kind === 'error')
            throw new RemoteError(typeof result.message === 'string' ? result.message : 'RPC rejected', typeof result.code === 'string' && result.code.startsWith('ERR_CEDEGRID_') ? result.code : undefined);
        return result;
    }
    putPool(poolId: string, nodeIds: string[], options: {
        maxWorkers: IntegerInput;
        minWorkers?: IntegerInput;
        allocationClass?: string;
    }): Promise<RpcReply> { return this.request('put_pool', { pool: { pool_id: poolId, node_ids: nodeIds, class: options.allocationClass ?? 'guaranteed', min_workers: integer(options.minWorkers ?? 0n, 'minWorkers'), max_workers: integer(options.maxWorkers, 'maxWorkers') } }); }
    submit(jobId: string, poolId: string, tasks: Record<string, JsonValue>[], options: {
        priority?: IntegerInput;
    } = {}): Promise<RpcReply> { return this.request('submit', { job: { job_id: jobId, pool_id: poolId, tasks, priority: integer(options.priority ?? 0n, 'priority', -2147483648n, 2147483647n) } }); }
    status(jobId?: string): Promise<RpcReply> { return this.request('status', { job_id: jobId ?? null }); }
    statusPage(collection: StatusCollection, filters: StatusFilters = {}): Promise<RpcReply> { return this.request('status_page', { collection, job_id: filters.jobId ?? null, node_id: filters.nodeId ?? null, pool_id: filters.poolId ?? null, limit: integer(filters.limit ?? 100n, 'limit', 1n, 1000n), cursor: filters.cursor ?? null }); }
    async *iterStatus(collection: StatusCollection, filters: StatusFilters = {}): AsyncGenerator<RpcReply> { let cursor = filters.cursor; for (;;) {
        const page = await this.statusPage(collection, { ...filters, cursor });
        yield* page.items;
        const next = page.next_cursor;
        if (next === null || next === undefined)
            return;
        if (typeof next !== 'string' || !next || next === cursor)
            throw new RemoteError('status cursor did not advance', 'ERR_CEDEGRID_PROTOCOL');
        cursor = next;
    } }
    cancel(jobId: string): Promise<RpcReply> { return this.request('cancel', { job_id: jobId }); }
    drainNode(nodeId: string, drain = true): Promise<RpcReply> { return this.request('drain_node', { node_id: nodeId, drain }); }
    async result(taskId: string): Promise<RpcReply | null> { return (await this.request('get_result', { task_id: taskId })).submission; }
    resume(taskId: string, options: {
        sideEffectsReconciled?: boolean;
    } = {}): Promise<RpcReply> { return this.request('retry', { task_id: taskId, confirm_side_effects_reconciled: options.sideEffectsReconciled ?? false }); }
    async upload(path: string, assignmentId: string, generation: IntegerInput): Promise<Artifact> {
        const hash = createHash('sha256');
        for await (const chunk of createReadStream(path))
            hash.update(chunk);
        const artifact = { sha256: hash.digest('hex'), size: (await fs.stat(path, { bigint: true })).size };
        let reply = await this.request('begin_upload', { assignment_id: assignmentId, generation: integer(generation, 'generation', 1n, I64_MAX), artifact });
        let offset = integer(reply.offset, 'offset', 0n, artifact.size);
        const source = await fs.open(path, 'r');
        try {
            const buffer = Buffer.alloc(1024 * 1024);
            while (offset < artifact.size) {
                const { bytesRead } = await source.read(buffer, 0, Number(artifact.size - offset < 1048576n ? artifact.size - offset : 1048576n), offset);
                if (!bytesRead)
                    throw new RemoteError('artifact source changed');
                reply = await this.request('upload_chunk', { upload_id: reply.upload_id, offset, data_hex: buffer.subarray(0, bytesRead).toString('hex') });
                offset += BigInt(bytesRead);
                if (reply.offset !== offset)
                    throw new RemoteError('upload offset mismatch');
            }
        }
        finally {
            await source.close();
        }
        return (await this.request('commit_upload', { upload_id: reply.upload_id })).artifact;
    }
    async download(artifact: Artifact, destination: string, options: { durability?: 'strict' | 'portable' } = {}): Promise<string> {
        const durability = options.durability ?? 'strict';
        if (durability !== 'strict' && durability !== 'portable')
            throw new ValidationError('download durability must be strict or portable');
        if (durability === 'strict' && process.platform === 'win32')
            throw new ValidationError('strict download durability is unavailable on Windows; explicitly select portable');
        const size = integer(artifact.size, 'artifact size');
        if (typeof artifact.sha256 !== 'string' || !/^[0-9a-f]{64}$/.test(artifact.sha256))
            throw new ValidationError('artifact SHA256 must be lowercase hexadecimal');
        await downloadDirectory(dirname(destination), durability === 'strict');
        const operationId = randomUUID();
        const temporary = join(dirname(destination), `.${basename(destination)}.${operationId}.download`);
        const target = await fs.open(temporary, 'wx');
        let offset = 0n;
        const hash = createHash('sha256');
        try {
            while (offset < size) {
                const reply = await this.request('read_artifact', { sha256: artifact.sha256, offset, max_bytes: size - offset < 1048576n ? size - offset : 1048576n });
                if (typeof reply.data_hex !== 'string' || !/^([a-fA-F0-9]{2})*$/.test(reply.data_hex))
                    throw new RemoteError('invalid artifact chunk');
                const chunk = Buffer.from(reply.data_hex, 'hex');
                if (!chunk.length || offset + BigInt(chunk.length) > size)
                    throw new RemoteError('invalid artifact chunk');
                await target.writeFile(chunk);
                hash.update(chunk);
                offset += BigInt(chunk.length);
            }
            await target.sync();
            await target.close();
            if (hash.digest('hex') !== artifact.sha256)
                throw new RemoteError('artifact integrity failure');
            let published = false;
            try {
                try {
                    await fs.link(temporary, destination);
                    published = true;
                }
                catch (error) {
                    if ((error as NodeJS.ErrnoException).code !== 'EEXIST')
                        throw error;
                    const info = await fs.lstat(destination);
                    if (!info.isFile())
                        throw new ValidationError('destination is not a regular file');
                    const existing = createHash('sha256');
                    for await (const chunk of createReadStream(destination))
                        existing.update(chunk);
                    if (existing.digest('hex') !== artifact.sha256)
                        throw new ValidationError('immutable download conflict');
                }
                await fs.unlink(temporary);
                if (durability === 'strict') await syncDirectory(dirname(destination));
            } catch (cause) {
                if (published) throw new DownloadUncertain(destination, operationId, artifact.sha256, { cause });
                throw cause;
            }
            return destination;
        }
        finally {
            await target.close().catch(() => { });
            await fs.unlink(temporary).catch(() => { });
        }
    }
}
