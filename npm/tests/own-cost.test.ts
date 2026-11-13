/** CedeGrid's own cost inside a worker's process, mirroring python/tests/test_own_cost.py. */
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, writeFileSync, rmSync, utimesSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { ResourceSignals } from 'cedegrid';

const CHECKS_PER_SECOND = 20;

function perCall(call: () => unknown, count: number): number {
    const began = process.cpuUsage();
    for (let i = 0; i < count; i++) call();
    const used = process.cpuUsage(began);
    return (used.user + used.system) / 1e6 / count;
}

test('a drain check and a target read cost under 1% of a core at 20 checks a second', () => {
    const root = mkdtempSync(join(tmpdir(), 'cedegrid-own-cost-'));
    try {
        const target = join(root, 'resource-target.json');
        writeFileSync(target, JSON.stringify({ version: 1, sequence: 1, deadline_unix_ms: 1793000000000, step: 'hint', targets: { ram_mib: 64 }, allowance: { ram_mib: 128 }, allowance_sequence: 1 }));
        const old = Date.now() / 1000 - 10;
        utimesSync(target, old, old);
        const signals = new ResourceSignals(join(root, 'drain'), target);
        signals.draining();
        const check = perCall(() => signals.draining(), 20000);
        const fresh = new ResourceSignals(join(root, 'drain'), target);
        const read = perCall(() => { (fresh as unknown as { identity: unknown }).identity = undefined; return fresh.resourceTargets(); }, 5000);
        const share = check * CHECKS_PER_SECOND;
        console.log(JSON.stringify({ sdk: 'typescript', drain_check_us: +(check * 1e6).toFixed(2), target_read_us: +(read * 1e6).toFixed(2), core_share_at_20_per_second: +share.toFixed(6) }));
        assert.ok(share < 0.01, `share ${share}`);
    } finally { rmSync(root, { recursive: true, force: true }); }
});
