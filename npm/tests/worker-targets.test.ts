/** The resource-target channel (contracts/resource-target-file.md), mirroring the Python tests. */
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, writeFileSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { WorkerContext, ResourceSignals } from 'cedegrid';
import type { RpcReply, SupervisorTransport, ResourceTargets } from 'cedegrid';
import nodeFs = require('node:fs');

const transport: SupervisorTransport = { async request(): Promise<RpcReply> { throw new Error('unused'); } };

function fixture() {
    const root = mkdtempSync(join(tmpdir(), 'cedegrid-targets-'));
    const targetFile = join(root, 'resource-target.json');
    const context = new WorkerContext({ schema_version: 2n, namespace_id: 'namespace', session_id: 'session', task_id: 'task', assignment_id: 'assignment', generation: 2n, drain_file: join(root, 'drain'), resource_target_file: targetFile, capabilities: ['resource_targets'] }, transport);
    const write = (sequence: number, targets: Record<string, unknown> = {}, version = 1) => writeFileSync(targetFile, JSON.stringify({ version, sequence, deadline_unix_ms: 1793000000000, step: 'hint', targets, future_key: true }));
    const ack = () => readFileSync(join(root, 'resource-target.ack'), 'utf8').trim();
    return { root, targetFile, context, write, ack, done: () => rmSync(root, { recursive: true, force: true }) };
}

test('resource targets read the current file', () => {
    const f = fixture();
    try {
        assert.equal(f.context.resourceTargets(), null);
        f.write(3, { ram_mib: 6144, gpu_memory_mib: { 'GPU-a': 10240 } });
        const targets = f.context.resourceTargets()!;
        assert.deepEqual([targets.sequence, targets.deadlineUnixMs, targets.step], [3n, 1793000000000n, 'hint']);
        assert.deepEqual([targets.ramMib, targets.cpuMillicores, targets.gpuMemoryMib], [6144n, undefined, { 'GPU-a': 10240n }]);
        // A relaxed target is a new file with a higher sequence.
        f.write(4);
        const relaxed = f.context.resourceTargets()!;
        assert.deepEqual([relaxed.sequence, relaxed.ramMib], [4n, undefined]);
    }
    finally { f.done(); }
});

test('hooks fire once per sequence during drain polls', () => {
    const f = fixture();
    try {
        const seen: bigint[] = [];
        f.context.onResourceTarget((targets: ResourceTargets) => seen.push(targets.sequence));
        assert.equal(f.context.draining(), false);
        f.write(1, { ram_mib: 100 });
        f.context.draining(); f.context.safePoint(); f.context.draining();
        f.write(2, { ram_mib: 50 });
        f.context.draining();
        assert.deepEqual(seen, [1n, 2n]);
    }
    finally { f.done(); }
});

test('a late subscriber receives the current target once', () => {
    const f = fixture();
    try {
        f.write(4, { ram_mib: 100 });
        assert.equal(f.context.draining(), false);
        const seen: bigint[] = [];
        f.context.onResourceTarget((targets: ResourceTargets) => seen.push(targets.sequence));
        assert.deepEqual(seen, [4n]);
        f.context.draining(); f.context.safePoint();
        assert.deepEqual(seen, [4n]);
        f.write(5, { ram_mib: 50 });
        f.context.draining();
        assert.deepEqual(seen, [4n, 5n]);
    }
    finally { f.done(); }
});

test('hook errors are isolated', () => {
    const f = fixture();
    const warnings: unknown[] = [];
    const original = console.warn;
    console.warn = (...args: unknown[]) => { warnings.push(args); };
    try {
        const seen: bigint[] = [];
        f.context.onResourceTarget(() => { throw new Error('worker hook failed'); });
        f.context.onResourceTarget((targets: ResourceTargets) => seen.push(targets.sequence));
        f.write(1, { ram_mib: 100 });
        assert.equal(f.context.draining(), false);
        assert.deepEqual(seen, [1n]);
        assert.equal(warnings.length, 1);
    }
    finally { console.warn = original; f.done(); }
});

test('signals come from the environment alone', () => {
    const f = fixture();
    const keys = ['CEDEGRID_RESOURCE_TARGET_FILE', 'CEDEGRID_DRAIN_FILE'];
    const previous = keys.map(key => process.env[key]);
    try {
        f.write(5, { cpu_millicores: 2000 });
        process.env.CEDEGRID_RESOURCE_TARGET_FILE = f.targetFile;
        process.env.CEDEGRID_DRAIN_FILE = join(f.root, 'drain');
        const signals = ResourceSignals.fromEnv();
        assert.equal(signals.resourceTargets()!.cpuMillicores, 2000n);
        assert.equal(signals.draining(), false);
        writeFileSync(join(f.root, 'drain'), '');
        assert.equal(signals.draining(), true);
        for (const key of keys) delete process.env[key];
        const empty = ResourceSignals.fromEnv();
        assert.equal(empty.resourceTargets(), null);
        assert.equal(empty.draining(), false);
    }
    finally {
        keys.forEach((key, i) => previous[i] === undefined ? delete process.env[key] : process.env[key] = previous[i]);
        f.done();
    }
});

test('a newer file version is treated as absent', () => {
    const f = fixture();
    try {
        f.write(9, { ram_mib: 1 }, 2);
        assert.equal(f.context.resourceTargets(), null);
        for (const bad of ['{', '[]', '{"version":1}', '{"version":1,"sequence":-1,"deadline_unix_ms":0,"step":"hint","targets":{}}']) {
            writeFileSync(f.targetFile, bad);
            assert.equal(f.context.resourceTargets(), null, bad);
        }
    }
    finally { f.done(); }
});

test('the ack records the highest sequence seen', () => {
    const f = fixture();
    try {
        f.context.draining();
        assert.equal(f.ack(), '0');
        f.write(7, { ram_mib: 1 });
        f.context.resourceTargets();
        assert.equal(f.ack(), '7');
        // An older file never lowers the ack.
        f.write(6, { ram_mib: 1 });
        f.context.resourceTargets();
        assert.equal(f.ack(), '7');
    }
    finally { f.done(); }
});

test('a failed ack write is logged once and retried later', { skip: process.platform === 'win32' || process.getuid?.() === 0 }, (t) => {
    // One warning per failure, and the next attempt waits for the retry interval.
    const f = fixture();
    try {
        const warn = t.mock.method(console, 'warn', () => { });
        t.mock.timers.enable({ apis: ['Date'], now: Date.now() });
        f.write(3, { ram_mib: 100 });
        nodeFs.chmodSync(f.root, 0o500);
        for (let i = 0; i < 3; i++)
            assert.equal(f.context.resourceTargets()!.sequence, 3n);
        assert.equal(warn.mock.callCount(), 1);
        nodeFs.chmodSync(f.root, 0o700);
        // Writable again, but inside the retry interval: no attempt yet.
        f.context.resourceTargets();
        assert.equal(nodeFs.existsSync(join(f.root, 'resource-target.ack')), false);
        t.mock.timers.tick(1001);
        f.context.resourceTargets();
        assert.equal(f.ack(), '3');
    }
    finally {
        nodeFs.chmodSync(f.root, 0o700);
        f.done();
    }
});

test('allowance hooks fire now and on each change, apart from targets', () => {
    const f = fixture();
    try {
        const targets: bigint[] = [];
        const allowances: Array<[bigint, bigint | undefined]> = [];
        f.context.onResourceTarget(t => targets.push(t.sequence));
        f.context.onResourceAllowance(a => allowances.push([a.sequence, a.ramMib]));
        assert.deepEqual([targets, allowances], [[], []]);
        const file: Record<string, unknown> = { version: 1, sequence: 0, deadline_unix_ms: 0, step: 'none', targets: {}, allowance: { ram_mib: 20480 }, allowance_sequence: 1 };
        writeFileSync(f.targetFile, JSON.stringify(file));
        f.context.draining();
        // An allowance without a target calls no target hook; the ack still starts at 0.
        assert.deepEqual([targets, allowances], [[], [[1n, 20480n]]]);
        assert.equal(f.context.resourceTargets(), null);
        assert.equal(f.ack(), '0');
        // An allowance-only change calls only the allowance hook.
        Object.assign(file, { allowance: { ram_mib: 16384 }, allowance_sequence: 2 });
        writeFileSync(f.targetFile, JSON.stringify(file));
        f.context.draining();
        f.context.draining();
        assert.deepEqual([targets, allowances], [[], [[1n, 20480n], [2n, 16384n]]]);
        // A target change calls only the target hook, and the ack follows it.
        Object.assign(file, { sequence: 1, step: 'hint', targets: { ram_mib: 8192 }, deadline_unix_ms: 1793000000000 });
        writeFileSync(f.targetFile, JSON.stringify(file));
        f.context.draining();
        assert.deepEqual([targets, allowances], [[1n], [[1n, 20480n], [2n, 16384n]]]);
        assert.equal(f.ack(), '1');
        // A late subscriber receives the current allowance at once.
        const late: bigint[] = [];
        f.context.onResourceAllowance(a => late.push(a.sequence));
        assert.deepEqual(late, [2n]);
        assert.equal(f.context.resourceAllowance()!.ramMib, 16384n);
    }
    finally { f.done(); }
});

test('save-progress hooks fire once per new value, apart from targets', () => {
    const f = fixture();
    try {
        const targets: bigint[] = [];
        const allowances: bigint[] = [];
        const saves: bigint[] = [];
        f.context.onResourceTarget(t => targets.push(t.sequence));
        f.context.onResourceAllowance(a => allowances.push(a.sequence));
        f.context.onSaveProgress(() => { throw new Error('hook failed'); });
        f.context.onSaveProgress(sequence => saves.push(sequence));
        const file: Record<string, unknown> = { version: 1, sequence: 0, deadline_unix_ms: 0, step: 'none', targets: {}, save_progress_sequence: 1 };
        writeFileSync(f.targetFile, JSON.stringify(file));
        f.context.draining();
        f.context.draining();
        // A save-progress request is no target and no allowance; a failing hook stops nothing.
        assert.deepEqual([targets, allowances, saves], [[], [], [1n]]);
        assert.equal(f.context.resourceTargets(), null);
        Object.assign(file, { allowance: { ram_mib: 20480 }, allowance_sequence: 1 });
        writeFileSync(f.targetFile, JSON.stringify(file));
        f.context.draining();
        assert.deepEqual([targets, allowances, saves], [[], [1n], [1n]]);
        Object.assign(file, { save_progress_sequence: 2 });
        writeFileSync(f.targetFile, JSON.stringify(file));
        f.context.draining();
        f.context.draining();
        assert.deepEqual(saves, [1n, 2n]);
        // A late subscriber is not asked again for a request already seen.
        const late: bigint[] = [];
        f.context.onSaveProgress(sequence => late.push(sequence));
        f.context.draining();
        assert.deepEqual(late, []);
        assert.throws(() => f.context.onSaveProgress(undefined as unknown as () => void));
    }
    finally { f.done(); }
});

test('target file interop cases', () => {
    const cases = JSON.parse(readFileSync(join(__dirname, '..', '..', '..', 'tests', 'contracts', 'cedegrid-0.2.json'), 'utf8')).target_file_cases;
    const f = fixture();
    try {
        const number = (value?: bigint) => value === undefined ? null : Number(value);
        const gpu = (amounts: Record<string, bigint>) => Object.fromEntries(Object.entries(amounts).map(([k, v]) => [k, Number(v)]));
        for (const c of cases) {
            writeFileSync(f.targetFile, JSON.stringify(c.file));
            const signals = new ResourceSignals(undefined, f.targetFile);
            const t = signals.resourceTargets();
            const a = signals.resourceAllowance();
            assert.deepEqual(t && { sequence: Number(t.sequence), deadline_unix_ms: Number(t.deadlineUnixMs), step: t.step, cpu_millicores: number(t.cpuMillicores), ram_mib: number(t.ramMib), gpu_memory_mib: gpu(t.gpuMemoryMib) }, c.targets, c.id);
            assert.deepEqual(a && { sequence: Number(a.sequence), cpu_millicores: number(a.cpuMillicores), ram_mib: number(a.ramMib), gpu_memory_mib: gpu(a.gpuMemoryMib) }, c.allowance, c.id);
            assert.equal(Number(signals.saveProgressSequence()), c.save_progress, c.id);
        }
    }
    finally { f.done(); }
});

test('worker context cases', () => {
    const cases = JSON.parse(readFileSync(join(__dirname, '..', '..', '..', 'tests', 'contracts', 'cedegrid-0.2.json'), 'utf8')).worker_context_cases;
    for (const c of cases) {
        const context = { ...c.context, schema_version: BigInt(c.context.schema_version), generation: BigInt(c.context.generation) };
        if (!c.accepted) {
            assert.throws(() => new WorkerContext(context, transport), Error, c.id);
            continue;
        }
        const worker = new WorkerContext(context, transport);
        assert.equal(worker.context.resource_target_file ?? null, c.resource_target_file, c.id);
        assert.deepEqual(worker.context.capabilities ?? [], c.capabilities, c.id);
    }
});

/** Write a target the way the supervisor does: a new file renamed over the old one. */
function replaceTarget(root: string, targetFile: string, sequence: number, targets: Record<string, unknown>, mtime?: Date) {
    const temporary = join(root, '.resource-target.json.tmp');
    writeFileSync(temporary, JSON.stringify({ version: 1, sequence, deadline_unix_ms: 1793000000000, step: 'hint', targets }));
    if (mtime)
        nodeFs.utimesSync(temporary, mtime, mtime);
    nodeFs.renameSync(temporary, targetFile);
}

test('an unchanged target file is not read again', (t) => {
    const f = fixture();
    try {
        // Older than 50 ms: a recent file is parsed again (R5).
        const old = () => new Date(Date.now() - 1000);
        replaceTarget(f.root, f.targetFile, 1, { ram_mib: 100 }, old());
        const signals = new ResourceSignals(undefined, f.targetFile);
        const reads = t.mock.method(nodeFs, 'readFileSync');
        const targetReads = () => reads.mock.calls.filter((call) => call.arguments[0] === f.targetFile).length;
        for (let i = 0; i < 50; i++) { signals.poll(); signals.draining(); }
        assert.equal(targetReads(), 1);
        replaceTarget(f.root, f.targetFile, 2, { ram_mib: 50 }, old());
        assert.equal(signals.poll()!.sequence, 2n);
        assert.equal(targetReads(), 2);
    }
    finally { f.done(); }
});

test('a failed read is not cached', () => {
    const f = fixture();
    try {
        replaceTarget(f.root, f.targetFile, 1, { ram_mib: 100 }, new Date(Date.now() - 1000));
        const signals = new ResourceSignals(undefined, f.targetFile);
        nodeFs.chmodSync(f.targetFile, 0);
        let unreadable;
        try { unreadable = signals.poll(); }
        finally { nodeFs.chmodSync(f.targetFile, 0o600); }
        if (unreadable === null) {
            // The stat identity is unchanged; the next call reads again.
            assert.equal(signals.poll()!.sequence, 1n);
        }
    }
    finally { f.done(); }
});

test('a rewrite with a reused inode is seen while recent', () => {
    const f = fixture();
    try {
        // Two writes in place keep the inode; equal size and one mtime keep the whole identity.
        const stamp = new Date(Math.floor(Date.now() / 1000) * 1000 + 30_000);
        const rewrite = (sequence: number, ramMib: number) => {
            writeFileSync(f.targetFile, JSON.stringify({ version: 1, sequence, deadline_unix_ms: 1793000000000, step: 'hint', targets: { ram_mib: ramMib } }));
            nodeFs.utimesSync(f.targetFile, stamp, stamp);
        };
        rewrite(1, 100);
        const signals = new ResourceSignals(undefined, f.targetFile);
        assert.equal(signals.poll()!.sequence, 1n);
        const before = nodeFs.statSync(f.targetFile, { bigint: true });
        rewrite(2, 200);
        const after = nodeFs.statSync(f.targetFile, { bigint: true });
        assert.deepEqual([after.ino, after.size, after.mtimeNs], [before.ino, before.size, before.mtimeNs]);
        const seen = signals.poll()!;
        assert.deepEqual([seen.sequence, seen.ramMib], [2n, 200n]);
    }
    finally { f.done(); }
});

test('a same-size target renamed within one mtime tick is seen', () => {
    const f = fixture();
    try {
        // A whole second, so both files carry exactly the same modification time.
        const stamp = new Date(Math.floor(Date.now() / 1000) * 1000);
        replaceTarget(f.root, f.targetFile, 1, { ram_mib: 100 }, stamp);
        const signals = new ResourceSignals(undefined, f.targetFile);
        const seen: Array<[bigint, bigint | undefined]> = [];
        signals.onResourceTarget((targets: ResourceTargets) => seen.push([targets.sequence, targets.ramMib]));
        const before = nodeFs.statSync(f.targetFile, { bigint: true });
        replaceTarget(f.root, f.targetFile, 2, { ram_mib: 200 }, stamp);
        const after = nodeFs.statSync(f.targetFile, { bigint: true });
        assert.deepEqual([after.size, after.mtimeNs], [before.size, before.mtimeNs]);
        signals.poll(); signals.poll();
        assert.deepEqual(seen, [[1n, 100n], [2n, 200n]]);
    }
    finally { f.done(); }
});
