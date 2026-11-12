/** Drain hooks and managed children, mirroring the Python SDK tests. */
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createServer, Socket } from 'node:net';
import { mkdtempSync, mkdirSync, chmodSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { randomUUID } from 'node:crypto';
import { WorkerContext, ManagedChild, spawnManaged, DrainRequested, SpawnUncertain, PublicationUncertain, RemoteError, DeadlineExceeded, ValidationError, parseJson, stringifyJson } from 'cedegrid';
import type { RpcReply, SupervisorTransport } from 'cedegrid';

const descriptor = { schema_version: 2n, namespace_id: 'namespace', session_id: 'session', task_id: 'task', assignment_id: 'assignment', generation: 2n };

class FakeTransport implements SupervisorTransport {
    readonly calls: Array<[string, Record<string, unknown>]> = [];
    constructor(private readonly replies: RpcReply[]) { }
    async request(op: string, fields: Record<string, unknown> = {}): Promise<RpcReply> {
        this.calls.push([op, fields]);
        const reply = this.replies.shift();
        if (!reply) throw new Error('no scripted reply');
        return reply;
    }
}

/** Serve scripted supervisor replies on a private socket; `null` drops the reply. */
async function withSupervisor(replies: Array<RpcReply | null>, body: (requests: any[]) => Promise<void>): Promise<void> {
    const socketPath = join('/tmp', `cg-${randomUUID().slice(0, 12)}.sock`);
    const requests: any[] = [];
    const server = createServer({ allowHalfOpen: true }, (socket: Socket) => {
        let input = '';
        socket.on('data', chunk => {
            input += chunk.toString();
            if (!input.endsWith('\n')) return;
            requests.push(parseJson(input));
            const reply = replies.shift();
            if (reply === null || reply === undefined) socket.end();
            else socket.end(stringifyJson(reply) + '\n');
        });
        socket.on('error', () => { });
    });
    const environment = { CEDEGRID_SUPERVISOR_SOCKET: socketPath, CEDEGRID_SUPERVISOR_TOKEN: 'private-token', CEDEGRID_NAMESPACE_ID: 'namespace', CEDEGRID_SESSION_ID: 'session', CEDEGRID_ASSIGNMENT_ID: 'assignment', CEDEGRID_ATTEMPT_GENERATION: '9007199254740993', CEDEGRID_CONTEXT: undefined };
    const previous = Object.fromEntries(Object.keys(environment).map(key => [key, process.env[key]]));
    await new Promise<void>(done => server.listen(socketPath, done));
    try {
        for (const [key, value] of Object.entries(environment))
            value === undefined ? delete process.env[key] : process.env[key] = value;
        await body(requests);
    }
    finally {
        for (const [key, value] of Object.entries(previous))
            value === undefined ? delete process.env[key] : process.env[key] = value;
        await new Promise<void>(done => server.close(() => done()));
    }
}

test('a raising drain hook is isolated', (t) => {
    const root = mkdtempSync(join(tmpdir(), 'cedegrid-drain-'));
    try {
        const warn = t.mock.method(console, 'warn', () => { });
        const context = new WorkerContext({ ...descriptor, drain_file: join(root, 'drain') }, new FakeTransport([]));
        const calls: number[] = [];
        context.onDrain(() => { throw new Error('drain hook failed'); });
        context.onDrain(() => calls.push(2));
        writeFileSync(join(root, 'drain'), '');
        assert.throws(() => context.safePoint(), DrainRequested);
        assert.deepEqual(calls, [2]);
        assert.equal(warn.mock.callCount(), 1);
        assert.throws(() => context.safePoint(), DrainRequested);
        assert.deepEqual(calls, [2]);
    }
    finally {
        rmSync(root, { recursive: true, force: true });
    }
});

test('an unreadable drain path is checked again', { skip: process.platform === 'win32' || process.getuid?.() === 0 }, (t) => {
    // An error other than not-found is no drain for now, logged once (SDK-3).
    const root = mkdtempSync(join(tmpdir(), 'cedegrid-drain-'));
    const directory = join(root, 'private');
    try {
        const warn = t.mock.method(console, 'warn', () => { });
        mkdirSync(directory);
        const context = new WorkerContext({ ...descriptor, drain_file: join(directory, 'drain') }, new FakeTransport([]));
        writeFileSync(join(directory, 'drain'), '');
        chmodSync(directory, 0);
        assert.equal(context.draining(), false);
        assert.equal(context.draining(), false);
        context.safePoint();
        assert.equal(warn.mock.callCount(), 1);
        chmodSync(directory, 0o700);
        assert.throws(() => context.safePoint(), DrainRequested);
    }
    finally {
        chmodSync(directory, 0o700);
        rmSync(root, { recursive: true, force: true });
    }
});

test('drain hooks run once', () => {
    const root = mkdtempSync(join(tmpdir(), 'cedegrid-drain-'));
    try {
        const context = new WorkerContext({ ...descriptor, drain_file: join(root, 'drain') }, new FakeTransport([]));
        const calls: number[] = [];
        context.onDrain(() => calls.push(1));
        assert.equal(context.draining(), false);
        writeFileSync(join(root, 'drain'), '');
        assert.throws(() => context.safePoint(), DrainRequested);
        assert.equal(context.draining(), true);
        assert.deepEqual(calls, [1]);
    }
    finally {
        rmSync(root, { recursive: true, force: true });
    }
});

test('a drain-only worker behaves as under 0.2 beside a changing target file', () => {
    const root = mkdtempSync(join(tmpdir(), 'cedegrid-drain-'));
    const target = join(root, 'resource-target.json');
    const previous = process.env.CEDEGRID_RESOURCE_TARGET_FILE;
    process.env.CEDEGRID_RESOURCE_TARGET_FILE = target;
    try {
        const files = [JSON.stringify({ version: 1, sequence: 1, deadline_unix_ms: 1793000000000, step: 'hint', targets: { ram_mib: 64 }, allowance: { ram_mib: 128 }, allowance_sequence: 1 }), JSON.stringify({ version: 2, sequence: 2 }), '{not json', null];
        for (const shape of [{}, { resource_target_file: target, capabilities: ['resource_targets'] }]) {
            const drain = join(root, 'drain');
            rmSync(drain, { force: true });
            const transport = new FakeTransport([]);
            const context = new WorkerContext({ ...descriptor, ...shape, drain_file: drain }, transport);
            const calls: number[] = [];
            context.onDrain(() => calls.push(1));
            for (const content of files) {
                if (content === null) rmSync(target, { force: true });
                else writeFileSync(target, content);
                assert.equal(context.draining(), false);
                context.safePoint();
            }
            writeFileSync(drain, '');
            assert.throws(() => context.safePoint(), DrainRequested);
            assert.equal(context.draining(), true);
            assert.deepEqual(calls, [1]);
            assert.deepEqual(transport.calls, []);
        }
    }
    finally {
        if (previous === undefined) delete process.env.CEDEGRID_RESOURCE_TARGET_FILE;
        else process.env.CEDEGRID_RESOURCE_TARGET_FILE = previous;
        rmSync(root, { recursive: true, force: true });
    }
});
test('spawning requires the child contract and a supervisor endpoint', async () => {
    await assert.rejects(spawnManaged(['child'], { noEscape: false, singleProcess: true }), ValidationError);
    await assert.rejects(spawnManaged(['child'], { noEscape: true, singleProcess: true, cwd: 'relative' }), ValidationError);
    for (const argv of [[], [''], ['bad\0arg']])
        await assert.rejects(spawnManaged(argv, { noEscape: true, singleProcess: true }), ValidationError);
    const previous = process.env.CEDEGRID_SUPERVISOR_SOCKET;
    delete process.env.CEDEGRID_SUPERVISOR_SOCKET;
    try {
        await assert.rejects(spawnManaged(['child'], { noEscape: true, singleProcess: true }), ValidationError);
    }
    finally {
        if (previous !== undefined) process.env.CEDEGRID_SUPERVISOR_SOCKET = previous;
    }
});

test('a lost spawn reply preserves the deduplication identity', async () => {
    await withSupervisor([null, { ok: true, child_id: 'c', state: 'running' }], async requests => {
        const error = await spawnManaged(['child'], { noEscape: true, singleProcess: true }).then(() => undefined, caught => caught);
        assert(error instanceof SpawnUncertain);
        const child = await spawnManaged(['child'], { noEscape: true, singleProcess: true, requestId: error.requestId });
        assert.equal(child.childId, 'c');
        assert.equal(requests[0].request_id, requests[1].request_id);
        assert.equal('pid' in requests[0], false);
        assert.equal(requests[0].generation, 9007199254740993n);
    });
});

test('a spawn reply without a child ID is uncertain and a rejection stays definite', async () => {
    await withSupervisor([{ ok: true, state: 'running' }, { ok: false, message: 'supervisor rejected request', code: 'ERR_CEDEGRID_REJECTED' }], async () => {
        await assert.rejects(spawnManaged(['child'], { noEscape: true, singleProcess: true }), SpawnUncertain);
        await assert.rejects(spawnManaged(['child'], { noEscape: true, singleProcess: true }), (error: unknown) => error instanceof RemoteError && !(error instanceof SpawnUncertain));
    });
});

test('a request over the size limit is rejected before sending, not reported as uncertain', async () => {
    // SDK-4: nothing was sent, so the outcome is known.
    const big = 'x'.repeat(3 * 1024 * 1024 + 1);
    await withSupervisor([], async requests => {
        await assert.rejects(spawnManaged(['child'], { noEscape: true, singleProcess: true, env: { BIG: big } }), (error: unknown) => error instanceof ValidationError && !(error instanceof SpawnUncertain));
        const worker = new WorkerContext(descriptor);
        await assert.rejects(worker.complete({ blob: big }), (error: unknown) => error instanceof ValidationError && !(error instanceof PublicationUncertain));
        assert.equal(requests.length, 0);
    });
});

test('timeouts and reconciliation never signal a child', async () => {
    for (const [reply, expected] of [[{ state: 'running' }, DeadlineExceeded], [{ state: 'needs_reconciliation' }, RemoteError]] as const) {
        const transport = new FakeTransport([reply]);
        await assert.rejects(new ManagedChild(transport, 'c').wait({ timeoutSeconds: 0 }), expected);
        assert.deepEqual(transport.calls.map(([op]) => op), ['status']);
    }
});

test('stop is a request and wait requires release', async () => {
    const transport = new FakeTransport([{ state: 'draining' }, { state: 'released', exit_code: 0n }]);
    const child = new ManagedChild(transport, 'c');
    assert.equal((await child.stop()).state, 'draining');
    assert.equal((await child.wait({ timeoutSeconds: 1 })).exit_code, 0n);
    assert.deepEqual(transport.calls, [['stop', { child_id: 'c' }], ['status', { child_id: 'c' }]]);
});
