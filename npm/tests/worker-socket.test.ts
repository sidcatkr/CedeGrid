/** Wire-level SDK fixture; durable native storage has separate Rust acceptance tests. */
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createServer } from 'node:net';
import { mkdtempSync, writeFileSync, rmSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createHash, randomUUID } from 'node:crypto';
import { WorkerContext, PublicationUncertain, parseJson, stringifyJson } from 'cedegrid';
test('worker streams bounded artifact chunks and reconciles a lost native ACK by publication ID', async () => {
    const root = mkdtempSync(join(tmpdir(), 'cedegrid-worker-'));
    const socketPath = join('/tmp', `cg-${randomUUID().slice(0, 12)}.sock`);
    const descriptor = { schema_version: 2n, namespace_id: 'namespace', session_id: 'session', task_id: 'task', assignment_id: 'assignment', generation: 3n, output_dir: join(root, 'never-created') };
    const contextPath = join(root, 'context.json');
    writeFileSync(contextPath, stringifyJson(descriptor));
    const data = Buffer.alloc(600 * 1024, 0x7f), source = join(root, 'source');
    writeFileSync(source, data);
    let bytes = Buffer.alloc(0), chunks = 0, sequence = 0n;
    const publications = new Map<string, any>();
    const server = createServer({ allowHalfOpen: true }, socket => {
        let input = '';
        socket.on('data', chunk => {
            input += chunk.toString();
            if (!input.endsWith('\n'))
                return;
            const request = parseJson(input) as any;
            try {
                assert.equal(request.version, 2n);
                assert.equal(request.token, 'private-token');
                assert.equal(request.namespace_id, 'namespace');
                assert.equal(request.session_id, 'session');
                assert.equal(request.assignment_id, 'assignment');
                assert.equal(request.generation, 3n);
                assert.equal(typeof request.request_id, 'string');
                let reply: any;
                if (request.op === 'artifact_begin') {
                    assert.equal(request.name, '结果');
                    assert.equal(request.size, BigInt(data.length));
                    reply = { upload_id: 'upload', offset: 0n };
                }
                else if (request.op === 'artifact_chunk') {
                    assert.equal(request.offset, BigInt(bytes.length));
                    const chunk = Buffer.from(request.data_hex, 'hex');
                    assert(chunk.length <= 256 * 1024);
                    chunks++;
                    bytes = Buffer.concat([bytes, chunk]);
                    reply = { upload_id: 'upload', offset: BigInt(bytes.length) };
                }
                else if (request.op === 'artifact_finish')
                    reply = { artifact_id: 'sealed', sha256: createHash('sha256').update(bytes).digest('hex'), size: BigInt(bytes.length) };
                else if (request.op === 'publication_commit') {
                    assert.equal(request.metadata.integer, 9007199254740993n);
                    assert.equal(typeof request.metadata.float, 'number');
                    reply = { publication_id: request.publication_id, state: 'committed', kind: request.kind, sequence: ++sequence, sha256: 'a'.repeat(64), assurance: 'durable_local' };
                    publications.set(request.publication_id, reply);
                    if (request.publication_id === 'lost-ack') {
                        socket.end();
                        return;
                    }
                }
                else if (request.op === 'publication_status')
                    reply = publications.get(request.publication_id);
                else
                    throw new Error('unexpected operation ' + request.op);
                socket.end(stringifyJson({ ok: true, ...reply }) + '\n');
            }
            catch (error) {
                socket.destroy(error as Error);
            }
        });
        socket.on('error', () => { });
    });
    const previous = { CEDEGRID_CONTEXT: process.env.CEDEGRID_CONTEXT, CEDEGRID_SUPERVISOR_SOCKET: process.env.CEDEGRID_SUPERVISOR_SOCKET, CEDEGRID_SUPERVISOR_TOKEN: process.env.CEDEGRID_SUPERVISOR_TOKEN };
    await new Promise<void>(done => server.listen(socketPath, done));
    try {
        Object.assign(process.env, { CEDEGRID_CONTEXT: contextPath, CEDEGRID_SUPERVISOR_SOCKET: socketPath, CEDEGRID_SUPERVISOR_TOKEN: 'private-token' });
        const worker = WorkerContext.fromEnv();
        const artifact = await worker.artifact('结果', source);
        assert.equal(chunks, 3);
        assert.deepEqual(bytes, data);
        const metadata = { integer: 9007199254740993n, float: 1.0 };
        const checkpoint = await worker.checkpoint(metadata, [artifact]);
        assert.equal(checkpoint.sequence, 1n);
        await assert.rejects(worker.complete(metadata, [artifact], { publicationId: 'lost-ack' }), (error: any) => error instanceof PublicationUncertain && error.publicationId === 'lost-ack' && error.identity.generation === 3n && !!error.requestDigest);
        assert.equal((await worker.publicationStatus('lost-ack')).state, 'committed');
        assert.equal(existsSync(descriptor.output_dir), false);
    }
    finally {
        for (const [key, value] of Object.entries(previous))
            value === undefined ? delete process.env[key] : process.env[key] = value;
        await new Promise<void>(done => server.close(() => done()));
        rmSync(root, { recursive: true, force: true });
    }
});
