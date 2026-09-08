import { createConnection } from 'node:net';
import { readFileSync, existsSync, promises as fs } from 'node:fs';
import { randomUUID, createHash } from 'node:crypto';
import { setTimeout as delay } from 'node:timers/promises';
import { performance } from 'node:perf_hooks';
import { parseJson, stringifyJson, integer, I64_MAX, JsonValue } from './codec';
import { argvField, textField, RpcReply } from './client';
import { CedeGridError, RemoteError, ValidationError, DeadlineExceeded, PublicationUncertain, SpawnUncertain, DrainRequested } from './errors';
export interface AttemptIdentity {
    namespace_id: string;
    session_id: string;
    assignment_id: string;
    generation: bigint;
}
export interface WorkerDescriptor extends AttemptIdentity {
    schema_version: bigint;
    task_id: string;
    output_dir?: string;
    drain_file?: string;
    inputs?: JsonValue[];
    resume?: JsonValue;
}
export interface SealedArtifact {
    name: string;
    artifact_id: string;
    sha256: string;
    size: bigint;
}
export interface PublicationHandle {
    publication_id: string;
    state: 'committed' | 'pending' | 'uncertain' | 'rejected';
    kind: 'checkpoint' | 'result';
    sequence?: bigint;
    sha256?: string;
    assurance?: 'durable_local' | 'replayable_local';
}
export interface SupervisorTransport {
    request(op: string, fields?: Record<string, unknown>): Promise<RpcReply>;
}
class ProtocolError extends CedeGridError {
    constructor(message: string, options?: ErrorOptions) { super(message, 'ERR_CEDEGRID_PROTOCOL', options); }
}
export class SupervisorClient implements SupervisorTransport {
    constructor(readonly endpoint: string, private readonly token: string, readonly identity: AttemptIdentity, readonly timeoutSeconds = 5) { }
    static fromEnv(context?: WorkerDescriptor): SupervisorClient {
        const endpoint = process.env.CEDEGRID_SUPERVISOR_SOCKET, token = process.env.CEDEGRID_SUPERVISOR_TOKEN;
        if (!endpoint || !token)
            throw new ValidationError('assignment does not expose a native supervisor endpoint');
        if (!context) {
            if (process.env.CEDEGRID_CONTEXT) context = readContext();
            else {
                const rawGeneration = process.env.CEDEGRID_ATTEMPT_GENERATION ?? '';
                if (!/^[0-9]+$/.test(rawGeneration)) throw new ValidationError('native supervisor attempt generation is required');
                context = {schema_version: 2n, namespace_id: process.env.CEDEGRID_NAMESPACE_ID!, session_id: process.env.CEDEGRID_SESSION_ID!, assignment_id: process.env.CEDEGRID_ASSIGNMENT_ID!, task_id: process.env.CEDEGRID_TASK_ID!, generation: integer(BigInt(rawGeneration), 'generation', 1n, I64_MAX)};
            }
        }
        for (const key of ['namespace_id', 'session_id', 'assignment_id'] as const) textField(context[key], key, true);
        const { namespace_id, session_id, assignment_id, generation } = context;
        return new SupervisorClient(endpoint, token, { namespace_id, session_id, assignment_id, generation });
    }
    async request(op: string, fields: Record<string, unknown> = {}): Promise<RpcReply> {
        const { request_id = randomUUID(), ...payload } = fields;
        const body = Buffer.from(stringifyJson({ version: 2n, op, request_id, token: this.token, ...this.identity, ...payload }) + '\n');
        const limit = 3 * 1024 * 1024;
        if (body.length > limit)
            throw new ValidationError('supervisor request exceeds 3 MiB');
        return new Promise((resolve, reject) => {
            const socket = createConnection(this.endpoint);
            let chunks: Buffer[] = [];
            let size = 0;
            let finished = false;
            const finish = (error?: unknown, value?: RpcReply) => { if (finished)
                return; finished = true; clearTimeout(timer); socket.destroy(); error ? reject(error) : resolve(value!); };
            const timer = setTimeout(() => finish(new DeadlineExceeded('supervisor request deadline exceeded')), this.timeoutSeconds * 1000);
            socket.on('connect', () => socket.end(body));
            socket.on('error', error => finish(error));
            socket.on('data', (chunk: Buffer) => {
                chunks.push(chunk);
                size += chunk.length;
                if (size > limit) {
                    finish(new ProtocolError('supervisor reply exceeds 3 MiB'));
                    return;
                }
                if (chunk.includes(10)) {
                    try {
                        const bytes = Buffer.concat(chunks);
                        if (bytes[bytes.length - 1] !== 10)
                            throw new ProtocolError('invalid supervisor frame');
                        const reply = parseJson(new TextDecoder('utf-8', { fatal: true }).decode(bytes));
                        if (!reply || typeof reply !== 'object' || Array.isArray(reply) || typeof reply.ok !== 'boolean')
                            throw new ProtocolError('invalid supervisor reply');
                        if (!reply.ok)
                            throw Object.assign(new RemoteError(typeof reply.message === 'string' ? reply.message : 'supervisor rejected request', typeof reply.code === 'string' ? reply.code : undefined), { details: reply });
                        finish(undefined, reply);
                    }
                    catch (cause) {
                        finish(cause instanceof RemoteError ? cause : new ProtocolError('invalid supervisor reply', { cause }));
                    }
                }
            });
            socket.on('end', () => { if (!finished)
                finish(new ProtocolError('supervisor acknowledgement missing')); });
        });
    }
}
function readContext(): WorkerDescriptor {
    const path = process.env.CEDEGRID_CONTEXT;
    if (!path)
        throw new ValidationError('CEDEGRID_CONTEXT is required');
    const context = parseJson(readFileSync(path, 'utf8')) as unknown as WorkerDescriptor;
    if (!context || context.schema_version !== 2n)
        throw new ValidationError('worker context version 2 is required');
    for (const key of ['namespace_id', 'session_id', 'assignment_id', 'task_id'] as const)
        textField(context[key], key, true);
    context.generation = integer(context.generation, 'generation', 1n, I64_MAX);
    return context;
}
export interface SpawnOptions {
    cwd?: string;
    env?: Record<string, string>;
    noEscape: boolean;
    singleProcess: boolean;
    requestId?: string;
}
export class ManagedChild {
    constructor(private readonly client: SupervisorTransport, readonly childId: string) { }
    status(): Promise<RpcReply> { return this.client.request('status', { child_id: this.childId }); }
    stop(): Promise<RpcReply> { return this.client.request('stop', { child_id: this.childId }); }
    async wait(options: {
        timeoutSeconds?: number;
        pollIntervalSeconds?: number;
    } = {}): Promise<RpcReply> {
        const interval = options.pollIntervalSeconds ?? 0.1;
        if (!Number.isFinite(interval) || interval <= 0 || options.timeoutSeconds !== undefined && (!Number.isFinite(options.timeoutSeconds) || options.timeoutSeconds < 0))
            throw new ValidationError('invalid child wait intervals');
        const deadline = options.timeoutSeconds === undefined ? Infinity : performance.now() + options.timeoutSeconds * 1000;
        for (;;) {
            const reply = await this.status();
            if (reply.state === 'released')
                return reply;
            if (reply.state === 'needs_reconciliation')
                throw new RemoteError('child remains reserved and requires reconciliation', 'ERR_CEDEGRID_RECONCILIATION_REQUIRED');
            if (performance.now() >= deadline)
                throw new DeadlineExceeded('child wait expired; child was not signaled');
            await delay(Math.min(interval * 1000, deadline - performance.now()));
        }
    }
}
export async function spawnManaged(argv: string[], options: SpawnOptions): Promise<ManagedChild> {
    argvField(argv);
    if (options.noEscape !== true || options.singleProcess !== true)
        throw new ValidationError('explicit singleProcess/noEscape acknowledgement required');
    if (options.cwd !== undefined && !/^(\/|[A-Za-z]:[\\/]|\\\\)/.test(options.cwd))
        throw new ValidationError('child cwd must be absolute');
    const client = SupervisorClient.fromEnv(), requestId = options.requestId ?? randomUUID();
    const fields: Record<string, unknown> = { request_id: requestId, argv, no_escape: true, single_process: true };
    if (options.cwd !== undefined)
        fields.cwd = options.cwd;
    if (options.env !== undefined)
        fields.env = options.env;
    try {
        const reply = await client.request('spawn', fields);
        if (typeof reply.child_id !== 'string' || !reply.child_id)
            throw new ProtocolError('spawn reply missing child_id');
        return new ManagedChild(client, reply.child_id);
    }
    catch (cause) {
        if (cause instanceof RemoteError)
            throw cause;
        throw new SpawnUncertain(requestId, { cause });
    }
}
export class WorkerContext {
    private readonly client: SupervisorTransport;
    private readonly artifacts = new Map<string, SealedArtifact>();
    private readonly drainHooks: Array<(context: WorkerContext) => void> = [];
    private drainNotified = false;
    constructor(readonly context: WorkerDescriptor, client?: SupervisorTransport) {
        if (context.schema_version !== 2n)
            throw new ValidationError('worker context version 2 is required');
        for (const key of ['namespace_id', 'session_id', 'assignment_id', 'task_id'] as const)
            textField(context[key], key, true);
        integer(context.generation, 'generation', 1n, I64_MAX);
        this.client = client ?? SupervisorClient.fromEnv(context);
    }
    static fromEnv(): WorkerContext { return new WorkerContext(readContext()); }
    get identity(): AttemptIdentity & {
        task_id: string;
    } { const { namespace_id, session_id, assignment_id, generation, task_id } = this.context; return { namespace_id, session_id, assignment_id, generation, task_id }; }
    get inputs(): JsonValue[] { return [...(this.context.inputs ?? [])]; }
    get resume(): JsonValue | undefined { return this.context.resume; }
    onDrain(callback: (context: WorkerContext) => void): void { this.drainHooks.push(callback); }
    draining(): boolean { const path = this.context.drain_file ?? process.env.CEDEGRID_DRAIN_FILE; const requested = !!path && existsSync(path); if (requested && !this.drainNotified) {
        this.drainNotified = true;
        for (const callback of this.drainHooks)
            callback(this);
    } return requested; }
    safePoint(): void { if (this.draining())
        throw new DrainRequested(); }
    async artifact(name: string, path: string, options: {
        requestId?: string;
    } = {}): Promise<SealedArtifact> {
        textField(name, 'artifact name', true);
        if (Buffer.byteLength(name) > 128 || name === '.' || name === '..' || /[\\/]/.test(name))
            throw new ValidationError('artifact name must be one path component');
        const source = await fs.open(path, 'r');
        try {
            const info = await source.stat({ bigint: true });
            if (!info.isFile())
                throw new ValidationError('artifact source must be a regular file');
            const begin = await this.client.request('artifact_begin', { request_id: options.requestId ?? randomUUID(), name, size: info.size });
            let offset = integer(begin.offset, 'artifact offset', 0n, info.size);
            const uploadId = begin.upload_id;
            const hash = createHash('sha256');
            const buffer = Buffer.alloc(256 * 1024);
            let position = 0n;
            for (;;) {
                const { bytesRead } = await source.read(buffer, 0, buffer.length, null);
                if (!bytesRead)
                    break;
                const chunk = buffer.subarray(0, bytesRead);
                hash.update(chunk);
                if (position + BigInt(bytesRead) > offset) {
                    const tail = chunk.subarray(Number(offset > position ? offset - position : 0n));
                    const reply = await this.client.request('artifact_chunk', { upload_id: uploadId, offset, data_hex: tail.toString('hex') });
                    offset += BigInt(tail.length);
                    if (reply.offset !== offset)
                        throw new ProtocolError('artifact offset mismatch');
                }
                position += BigInt(bytesRead);
            }
            const sealed = await this.client.request('artifact_finish', { upload_id: uploadId });
            if (position !== info.size || sealed.size !== info.size || sealed.sha256 !== hash.digest('hex') || typeof sealed.artifact_id !== 'string')
                throw new ProtocolError('artifact integrity failure');
            const artifact: SealedArtifact = { name, artifact_id: sealed.artifact_id, sha256: sealed.sha256, size: sealed.size };
            this.artifacts.set(artifact.artifact_id, { ...artifact });
            return artifact;
        }
        finally {
            await source.close();
        }
    }
    private async publish(kind: 'checkpoint' | 'result', metadata: JsonValue, artifacts: SealedArtifact[], publicationId: string = randomUUID()): Promise<PublicationHandle> {
        if (new Set(artifacts.map(a => a.name)).size !== artifacts.length)
            throw new ValidationError('artifact names must be unique');
        for (const artifact of artifacts)
            if (stringifyJson(this.artifacts.get(artifact.artifact_id) ?? null) !== stringifyJson(artifact))
                throw new ValidationError('artifact is not sealed by this context');
        const fields = { publication_id: publicationId, kind, metadata, artifact_ids: artifacts.map(a => a.artifact_id) };
        const digest = createHash('sha256').update(stringifyJson(fields)).digest('hex');
        let knownDigest: string | undefined;
        try {
            const reply = await this.client.request('publication_commit', { request_id: publicationId, ...fields });
            knownDigest = typeof reply.sha256 === 'string' ? reply.sha256 : undefined;
            if (reply.publication_id !== publicationId || !['committed', 'rejected'].includes(reply.state))
                throw new ProtocolError('publication durability acknowledgement missing');
            if (reply.state === 'committed' && (!['durable_local', 'replayable_local'].includes(reply.assurance) || !knownDigest || !/^[a-f0-9]{64}$/.test(knownDigest)))
                throw new ProtocolError('publication commit evidence missing');
            if (reply.state === 'rejected')
                throw new RemoteError(reply.message ?? 'publication rejected');
            return reply as PublicationHandle;
        }
        catch (cause) {
            if (cause instanceof RemoteError && cause.code !== 'ERR_CEDEGRID_PUBLICATION_UNCERTAIN')
                throw cause;
            if (cause instanceof RemoteError && 'details' in cause)
                knownDigest = (cause.details as any)?.sha256;
            throw new PublicationUncertain(publicationId, { ...this.identity }, knownDigest, { cause }, digest);
        }
    }
    checkpoint(metadata: JsonValue, artifacts: SealedArtifact[] = [], options: {
        publicationId?: string;
    } = {}): Promise<PublicationHandle> { return this.publish('checkpoint', metadata, artifacts, options.publicationId); }
    complete(metadata: JsonValue, artifacts: SealedArtifact[] = [], options: {
        publicationId?: string;
    } = {}): Promise<PublicationHandle> { return this.publish('result', metadata, artifacts, options.publicationId); }
    async publicationStatus(publicationId: string): Promise<PublicationHandle> { return await this.client.request('publication_status', { publication_id: publicationId }) as PublicationHandle; }
    async publicationAbort(publicationId: string): Promise<PublicationHandle> { return await this.client.request('publication_abort', { publication_id: publicationId }) as PublicationHandle; }
    spawnManaged(argv: string[], options: SpawnOptions): Promise<ManagedChild> { this.safePoint(); return spawnManaged(argv, options); }
}
