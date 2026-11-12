import { createConnection } from 'node:net';
import { readFileSync, writeFileSync, renameSync, statSync, promises as fs } from 'node:fs';
import { dirname, join } from 'node:path';
import { randomUUID, createHash } from 'node:crypto';
import { setTimeout as delay } from 'node:timers/promises';
import { performance } from 'node:perf_hooks';
import { parseJson, stringifyJson, canonicalJson, integer, I64_MAX, JsonValue } from './codec';
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
    resource_target_file?: string;
    capabilities?: string[];
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
        // A request refused before sending was never spawned: its outcome is known (SDK-4).
        if (cause instanceof RemoteError || cause instanceof ValidationError)
            throw cause;
        throw new SpawnUncertain(requestId, { cause });
    }
}
/** A "use less" target from the supervisor. Unset amounts carry no target. */
export interface ResourceTargets {
    sequence: bigint;
    deadlineUnixMs: bigint;
    step: string;
    cpuMillicores?: bigint;
    ramMib?: bigint;
    gpuMemoryMib: Record<string, bigint>;
}
/** What own work may grow to now (FR-071). Unset amounts carry none. */
export interface ResourceAllowance {
    sequence: bigint;
    cpuMillicores?: bigint;
    ramMib?: bigint;
    gpuMemoryMib: Record<string, bigint>;
}
const amount = (v: unknown): v is bigint => typeof v === 'bigint' && v >= 0n;
/** The per-resource amounts of a target or allowance object, or null. A null CPU or RAM
 * amount is unset, as in the Python SDK. */
function amounts(value: any): { cpu?: bigint; ram?: bigint; gpu: Record<string, bigint> } | null {
    if (!value || typeof value !== 'object' || Array.isArray(value))
        return null;
    const { gpu_memory_mib: gpu = {} } = value;
    const cpu = value.cpu_millicores ?? undefined, ram = value.ram_mib ?? undefined;
    if ((cpu !== undefined && !amount(cpu)) || (ram !== undefined && !amount(ram)) || !gpu || typeof gpu !== 'object' || Array.isArray(gpu) || !Object.values(gpu).every(amount))
        return null;
    return { cpu, ram, gpu: { ...gpu } };
}
type Signals = [ResourceTargets | null, ResourceAllowance | null, bigint];
/** A target file modified this recently is parsed again: one mtime tick can hide a rewrite (R5). */
const RECENT_NS = 50_000_000n;
/** After a failed ack write the next attempt waits this long, so a full disk is not retried
 * and logged on every read (SDK-8). */
const ACK_RETRY_MS = 1000;
/** Parse a target file into targets, allowance and the save-progress sequence, or null when it
 * could not be read, so the caller reads it again. An invalid or newer-version file is none of
 * them; sequence 0 means no target was published yet, and an invalid allowance or
 * save-progress sequence is ignored without losing the target. */
function readSignals(path: string): Signals | null {
    let text: string;
    try {
        text = readFileSync(path, 'utf8');
    }
    catch {
        return null;
    }
    let value: any;
    try {
        value = parseJson(text);
    }
    catch {
        return [null, null, 0n];
    }
    if (!value || typeof value !== 'object' || Array.isArray(value) || value.version !== 1n)
        return [null, null, 0n];
    const { sequence, deadline_unix_ms: deadline, step } = value;
    const targets = amounts(value.targets);
    if (!amount(sequence) || !amount(deadline) || typeof step !== 'string' || !targets)
        return [null, null, 0n];
    const allowance = amounts(value.allowance);
    const allowanceSequence = value.allowance_sequence ?? 0n;
    const saveProgress = value.save_progress_sequence ?? 0n;
    return [
        sequence > 0n ? { sequence, deadlineUnixMs: deadline, step, cpuMillicores: targets.cpu, ramMib: targets.ram, gpuMemoryMib: targets.gpu } : null,
        allowance && amount(allowanceSequence) && allowanceSequence > 0n ? { sequence: allowanceSequence, cpuMillicores: allowance.cpu, ramMib: allowance.ram, gpuMemoryMib: allowance.gpu } : null,
        amount(saveProgress) ? saveProgress : 0n,
    ];
}
/** Drain and resource-target signals from CEDEGRID_DRAIN_FILE and
 * CEDEGRID_RESOURCE_TARGET_FILE alone, so managed children can use them. */
export class ResourceSignals {
    private readonly hooks: Array<(targets: ResourceTargets) => void> = [];
    private readonly allowanceHooks: Array<(allowance: ResourceAllowance) => void> = [];
    private readonly saveHooks: Array<(sequence: bigint) => void> = [];
    private notified?: bigint;
    private allowanceNotified?: bigint;
    private saveNotified = 0n;
    private acked?: bigint;
    private ackFailedAt?: number;
    private identity?: string | null;
    private cached: Signals = [null, null, 0n];
    constructor(readonly drainFile?: string, readonly targetFile?: string) { }
    static fromEnv(): ResourceSignals { return new ResourceSignals(process.env.CEDEGRID_DRAIN_FILE || undefined, process.env.CEDEGRID_RESOURCE_TARGET_FILE || undefined); }
    /** Parse the file again when its stat identity changed or its mtime is within 50 ms of
     * now: the supervisor replaces it by rename, but two writes within one mtime tick can keep
     * the size and a reused inode (R5). The identity is kept only after a read that succeeded,
     * so a failed read is tried again (SDK-1). */
    private read(): Signals {
        if (!this.targetFile)
            return [null, null, 0n];
        let identity: string | null = null;
        let recent = false;
        try {
            const stat = statSync(this.targetFile, { bigint: true, throwIfNoEntry: false });
            identity = stat ? `${stat.dev}:${stat.ino}:${stat.size}:${stat.mtimeNs}` : null;
            recent = stat ? stat.mtimeNs > BigInt(Date.now()) * 1_000_000n - RECENT_NS : false;
        }
        catch {
            identity = null;
        }
        if (identity === null) {
            this.identity = null;
            this.cached = [null, null, 0n];
        }
        else if (identity !== this.identity || recent) {
            const read = readSignals(this.targetFile);
            if (read) {
                this.identity = identity;
                this.cached = read;
            }
            else if (identity !== this.identity) {
                this.identity = undefined;
                this.cached = [null, null, 0n];
            }
        }
        this.acknowledge(this.cached[0] ? this.cached[0].sequence : 0n);
        return this.cached;
    }
    resourceTargets(): ResourceTargets | null { return this.read()[0]; }
    resourceAllowance(): ResourceAllowance | null { return this.read()[1]; }
    /** How many save-progress requests the supervisor has published, 0n for none. */
    saveProgressSequence(): bigint { return this.read()[2]; }
    /** Call `callback` now with the current target, then once per new one. */
    onResourceTarget(callback: (targets: ResourceTargets) => void): void {
        if (typeof callback !== 'function')
            throw new ValidationError('resource-target callback must be a function');
        this.hooks.push(callback);
        const targets = this.resourceTargets();
        if (!targets)
            return;
        if (this.notified === undefined || targets.sequence > this.notified)
            this.poll();
        else
            ResourceSignals.call(callback, targets);
    }
    /** Call `callback` now with the current allowance, then once per new one.
     * Allowance changes never call target hooks, nor targets these. */
    onResourceAllowance(callback: (allowance: ResourceAllowance) => void): void {
        if (typeof callback !== 'function')
            throw new ValidationError('resource-allowance callback must be a function');
        this.allowanceHooks.push(callback);
        const allowance = this.resourceAllowance();
        if (!allowance)
            return;
        if (this.allowanceNotified === undefined || allowance.sequence > this.allowanceNotified)
            this.poll();
        else
            ResourceSignals.call(callback, allowance);
    }
    /** Call `callback(sequence)` once per new save-progress request (FR-075): save
     * progress now. It is not a drain and asks for no memory back. */
    onSaveProgress(callback: (sequence: bigint) => void): void {
        if (typeof callback !== 'function')
            throw new ValidationError('save-progress callback must be a function');
        this.saveHooks.push(callback);
        this.poll();
    }
    /** Read the file and call each hook once per new sequence of its kind. */
    poll(): ResourceTargets | null {
        const [targets, allowance, saveProgress] = this.read();
        if (targets && (this.notified === undefined || targets.sequence > this.notified)) {
            this.notified = targets.sequence;
            for (const callback of [...this.hooks])
                ResourceSignals.call(callback, targets);
        }
        if (allowance && (this.allowanceNotified === undefined || allowance.sequence > this.allowanceNotified)) {
            this.allowanceNotified = allowance.sequence;
            for (const callback of [...this.allowanceHooks])
                ResourceSignals.call(callback, allowance);
        }
        if (saveProgress > this.saveNotified && this.saveHooks.length > 0) {
            this.saveNotified = saveProgress;
            for (const callback of [...this.saveHooks])
                ResourceSignals.call(callback, saveProgress);
        }
        return targets;
    }
    /** A failing hook is logged and never stops the poll. */
    private static call<T>(callback: (value: T) => void, targets: T): void {
        try {
            callback(targets);
        }
        catch (error) {
            console.warn('resource-target hook failed', error);
        }
    }
    private drainUnreadable = false;
    /** Whether the drain file exists. An error other than not-found is no drain for now: it is
     * logged once and the path is checked again on the next call (SDK-3). */
    private drainRequested(): boolean {
        if (!this.drainFile)
            return false;
        try {
            const found = statSync(this.drainFile, { throwIfNoEntry: false }) !== undefined;
            this.drainUnreadable = false;
            return found;
        }
        catch (error) {
            if ((error as NodeJS.ErrnoException).code === 'ENOTDIR') {
                this.drainUnreadable = false;
                return false;
            }
            if (!this.drainUnreadable)
                console.warn('drain file could not be checked; it is checked again', error);
            this.drainUnreadable = true;
            return false;
        }
    }
    draining(): boolean { this.poll(); return this.drainRequested(); }
    /** Record the highest sequence seen; it shows the worker reads targets. */
    private acknowledge(sequence: bigint): void {
        if (!this.targetFile || (this.acked !== undefined && sequence <= this.acked))
            return;
        if (this.ackFailedAt !== undefined && Date.now() - this.ackFailedAt < ACK_RETRY_MS)
            return;
        const ack = join(dirname(this.targetFile), 'resource-target.ack');
        const temporary = join(dirname(this.targetFile), `.resource-target.ack.${process.pid}`);
        try {
            writeFileSync(temporary, `${sequence}\n`);
            renameSync(temporary, ack);
        }
        catch (error) {
            // Logged once per failure; later attempts are spaced and stay quiet until one succeeds.
            if (this.ackFailedAt === undefined)
                console.warn('resource-target ack could not be written; hints stay unavailable', error);
            this.ackFailedAt = Date.now();
            return;
        }
        this.acked = sequence;
        this.ackFailedAt = undefined;
    }
}
export class WorkerContext {
    private readonly client: SupervisorTransport;
    private readonly signals: ResourceSignals;
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
        // One precedence in both SDKs: the context's value, then the environment the supervisor
        // set; an empty value is unset (SDK-9).
        this.signals = new ResourceSignals(context.drain_file || process.env.CEDEGRID_DRAIN_FILE || undefined, context.resource_target_file || process.env.CEDEGRID_RESOURCE_TARGET_FILE || undefined);
    }
    static fromEnv(): WorkerContext { return new WorkerContext(readContext()); }
    get identity(): AttemptIdentity & {
        task_id: string;
    } { const { namespace_id, session_id, assignment_id, generation, task_id } = this.context; return { namespace_id, session_id, assignment_id, generation, task_id }; }
    get inputs(): JsonValue[] { return [...(this.context.inputs ?? [])]; }
    get resume(): JsonValue | undefined { return this.context.resume; }
    onDrain(callback: (context: WorkerContext) => void): void { this.drainHooks.push(callback); }
    resourceTargets(): ResourceTargets | null { return this.signals.resourceTargets(); }
    /** Call `callback(targets)` once per new target, during drain polls. */
    onResourceTarget(callback: (targets: ResourceTargets) => void): void { this.signals.onResourceTarget(callback); }
    resourceAllowance(): ResourceAllowance | null { return this.signals.resourceAllowance(); }
    /** Call `callback(allowance)` now, then once per new allowance, during drain polls (FR-071). */
    onResourceAllowance(callback: (allowance: ResourceAllowance) => void): void { this.signals.onResourceAllowance(callback); }
    /** Call `callback(sequence)` once per new save-progress request, during drain polls (FR-075). */
    onSaveProgress(callback: (sequence: bigint) => void): void { this.signals.onSaveProgress(callback); }
    draining(): boolean { const requested = this.signals.draining(); if (requested && !this.drainNotified) {
        this.drainNotified = true;
        // A failing hook is logged like a resource hook; the rest run and the drain stands.
        for (const callback of [...this.drainHooks]) {
            try {
                callback(this);
            }
            catch (error) {
                console.warn('drain hook failed', error);
            }
        }
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
        const digest = createHash('sha256').update(canonicalJson(fields)).digest('hex');
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
            // A request refused before sending published nothing (SDK-4).
            if (cause instanceof ValidationError || (cause instanceof RemoteError && cause.code !== 'ERR_CEDEGRID_PUBLICATION_UNCERTAIN'))
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
