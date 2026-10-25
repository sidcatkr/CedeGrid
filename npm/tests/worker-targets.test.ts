/** The resource-target channel (contracts/resource-target-file.md), mirroring the Python tests. */
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, writeFileSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { WorkerContext, ResourceSignals } from 'cedegrid';
import type { RpcReply, SupervisorTransport, ResourceTargets } from 'cedegrid';

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
