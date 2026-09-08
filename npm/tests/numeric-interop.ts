/** Run as a real supervised worker, or verify the other SDK's accepted result. */
import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { Client, WorkerContext, parseJson, stringifyJson, U64_MAX, I64_MIN } from 'cedegrid';
const metadata = { producer: 'typescript', integer53: 9007199254740993n, unsignedMax: U64_MAX, signedMin: I64_MIN, integerOne: 1n, floatOne: 1.0, negativeZero: -0, tiny: 5e-324, huge: 1.7976931348623157e308, nested: { unicode: '한국어 日本語 Ελληνικά', array: [9007199254740993n, 1.0, -0] } };
async function main(): Promise<void> {
    const [mode, ...args] = process.argv.slice(2);
    if (mode === 'worker') {
        const worker = WorkerContext.fromEnv();
        const directory = mkdtempSync(join(tmpdir(), 'cedegrid-numeric-worker-'));
        try {
            const path = join(directory, 'numbers.jsonl');
            writeFileSync(path, (stringifyJson(metadata) + '\n').repeat(2048));
            const artifact = await worker.artifact('숫자.jsonl', path);
            await worker.checkpoint(metadata, [artifact]);
            console.log(stringifyJson(await worker.complete(metadata, [artifact])));
        } finally { rmSync(directory, { recursive: true, force: true }); }
    }
    else if (mode === 'verify') {
        const [config, taskId, producer = 'python'] = args;
        if (!config || !taskId)
            throw new Error('verify CONFIG TASK_ID [PRODUCER]');
        const client = Client.fromConfig(config);
        const submission = await client.result(taskId);
        assert(submission, 'coordinator has not accepted the result');
        const value = submission.result.metadata;
        assert.equal(value.producer, producer);
        assert.equal(value.integer53, 9007199254740993n);
        assert.equal(value.unsignedMax, U64_MAX);
        assert.equal(value.signedMin, I64_MIN);
        assert.equal(value.integerOne, 1n);
        assert.equal(typeof value.floatOne, 'number');
        assert.equal(value.floatOne, 1);
        assert(Object.is(value.negativeZero, -0));
        assert.equal(value.tiny, 5e-324);
        assert.equal(value.huge, 1.7976931348623157e308);
        assert.equal(value.nested.unicode, metadata.nested.unicode);
        assert.equal(value.nested.array[0], 9007199254740993n);
        assert.equal(typeof value.nested.array[1], 'number');
        assert(Object.is(value.nested.array[2], -0));
        const directory = mkdtempSync(join(tmpdir(), 'cedegrid-numeric-receiver-'));
        try {
            assert.equal(submission.result.artifacts.length, 1);
            const path = join(directory, 'numbers.jsonl');
            await client.download(submission.result.artifacts[0], path);
            const bytes = readFileSync(path);
            assert(bytes.length > 256 * 1024);
            assert.equal((parseJson(bytes.toString().split('\n')[0]) as any).integer53, 9007199254740993n);
        } finally { rmSync(directory, { recursive: true, force: true }); }
        console.log(stringifyJson({ status: 'PASS', task_id: taskId, generation: submission.generation, producer, consumer: 'typescript', metadata: value }));
    }
    else if (mode === 'encode')
        console.log(stringifyJson(metadata));
    else
        throw new Error('expected worker, verify, or encode mode');
}
main().catch(error => { console.error(error); process.exitCode = 1; });
