import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, writeFileSync, readFileSync, rmSync, realpathSync, promises as fs } from 'node:fs';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { pathToFileURL } from 'node:url';
import { parseJson, stringifyJson, integer, U64_MAX, I64_MAX, commandTask, loadClientConfig, PublicationUncertain, DownloadUncertain, WorkerContext, Client, CedeGridError, isCedeGridError } from '../src';
import { createHash } from 'node:crypto';
import { endpointOrigin } from '../src/config';
import { nativePackage } from '../src/launcher';
import { protocolValue } from '../src/codec';
test('download synchronizes new ancestors and reports uncertainty after publication', { skip: process.platform === 'win32' }, async () => {
    const directory = realpathSync(mkdtempSync(join(tmpdir(), 'cedegrid-download-')));
    const payload = Buffer.from('exact artifact bytes');
    const artifact = { sha256: createHash('sha256').update(payload).digest('hex'), size: BigInt(payload.length) };
    const client = Object.create(Client.prototype) as Client;
    client.request = async () => ({ data_hex: payload.toString('hex') });
    const originalOpen = fs.open;
    const synchronized: string[] = [];
    let failDirectory = false;
    fs.open = (async (...args: Parameters<typeof fs.open>) => {
        const handle = await originalOpen(...args);
        if (args[1] === 'r') {
            const originalSync = handle.sync.bind(handle);
            handle.sync = async () => {
                synchronized.push(String(args[0]));
                if (failDirectory) throw new Error('directory sync failed');
                return originalSync();
            };
        }
        return handle;
    }) as typeof fs.open;
    try {
        const first = join(directory, '目录'), second = join(first, 'nested'), destination = join(second, 'out.bin');
        await client.download(artifact, destination);
        assert.deepEqual(synchronized, [first, directory, second, first, second]);
        assert.deepEqual(readFileSync(destination), payload);
        failDirectory = true;
        const uncertain = join(directory, 'uncertain.bin');
        await assert.rejects(client.download(artifact, uncertain), error => {
            assert(error instanceof DownloadUncertain);
            assert.equal(error.code, 'ERR_CEDEGRID_DOWNLOAD_UNCERTAIN');
            assert.equal(error.destination, uncertain);
            assert.equal(error.digest, artifact.sha256);
            assert(error.operationId);
            return true;
        });
        assert.deepEqual(readFileSync(uncertain), payload);
        await client.download(artifact, join(directory, 'portable', 'out.bin'), { durability: 'portable' });
    } finally { fs.open = originalOpen; rmSync(directory, { recursive: true, force: true }); }
});
test('exact numeric kinds, Unicode, extreme finite floats and negative zero', () => {
    const value = parseJson('{"nested":[9007199254740993,18446744073709551615,-9223372036854775808,1,1.0,-0,-0.0,5e-324,1.7976931348623157e308],"语言":"한국어"}') as any;
    assert.deepEqual(value.nested.slice(0, 3), [9007199254740993n, U64_MAX, -9223372036854775808n]);
    assert.equal(typeof value.nested[3], 'bigint');
    assert.equal(typeof value.nested[4], 'number');
    assert(Object.is(value.nested[5], -0));
    assert(Object.is(value.nested[6], -0));
    assert.deepEqual(parseJson(stringifyJson(value)), value);
    assert.match(stringifyJson(value), /1\.0/);
    assert.match(stringifyJson(value), /-0\.0/);
});
test('invalid values are rejected without silent rounding', () => {
    for (const text of ['18446744073709551616', '-9223372036854775809', '1e309', '1e-999', 'NaN', 'Infinity', '{"a":1,"a":2}', '"\\ud800"'])
        assert.throws(() => parseJson(text));
    for (const value of [NaN, Infinity, 18446744073709551616n, -9223372036854775809n, undefined, new Date()])
        assert.throws(() => stringifyJson(value));
    assert.equal(integer(U64_MAX), U64_MAX);
    for (const value of [true, 1.5, -1, 2 ** 53 + 1])
        assert.throws(() => integer(value));
    assert.throws(() => integer(I64_MAX + 1n, 'generation', 0n, I64_MAX));
    assert.equal(parseJson('0e-999'), 0);
});
test('prototype-like metadata keys and strings retain their exact values', () => {
    const text = '{"__proto__":{"polluted":true},"constructor":1,"$cedegrid$x":2,"value":"\\\"key\\\":42"}';
    const value = parseJson(text) as any;
    assert.equal(Object.getPrototypeOf(value), Object.prototype);
    assert.equal(Object.hasOwn(value, '__proto__'), true);
    assert.equal(value.__proto__.polluted, true);
    assert.equal(({} as any).polluted, undefined);
    assert.deepEqual(parseJson(stringifyJson(value)), value);
});
test('command task and generic RPC normalize wire integers without changing metadata', () => {
    const task = commandTask('task', ['worker'], '/work', { cpuMillicores: 1000, ramMib: 512, maxAttempts: 3 });
    assert.equal((task.resources as any).cpu_millicores, 1000n);
    const request = protocolValue({ generation: 5, report: { managed_budget: { cpu_millicores: 1000, gpu_memory_mib: { GPU: 64 } } }, metadata: { generation: 1.0 } }) as any;
    assert.equal(request.generation, 5n);
    assert.equal(request.report.managed_budget.gpu_memory_mib.GPU, 64n);
    assert.equal(typeof request.metadata.generation, 'number');
    for (const options of [{ cpuMillicores: true }, { maxAttempts: 0 }, { managedChildLimit: 1 }, { ramMib: 2 ** 53 + 1 }])
        assert.throws(() => commandTask('t', ['worker'], '/work', options as any));
});
test('strict HTTPS origin validation', () => {
    for (const origin of ['https://example.test', 'https://example.test/', 'https://[::1]:9443', 'https://localhost:65535'])
        assert.equal(endpointOrigin(origin), origin.replace(/\/$/, ''));
    for (const origin of ['http://example.test', 'https://example.test?', 'https://example.test#', 'https://@example.test', 'https://user@example.test', 'https://example.test/path', 'https://example.test:0', 'https://example.test:65536', 'https://example.test:', 'https://example.test\\path', 'https://example.test\n'])
        assert.throws(() => endpointOrigin(origin), { code: 'ERR_CEDEGRID_CONFIG' });
});
test('TOML 1.0 client configuration preserves large integer scalars', () => {
    const directory = realpathSync(mkdtempSync(join(tmpdir(), 'cedegrid-config-')));
    try {
        const path = join(directory, '配置.toml');
        const load = (fields: string) => { writeFileSync(path, 'config_version=1\nendpoint="https://example.test"\n' + fields + '\n[tls]\nca_cert="tls/ca.pem"\ncertificate="tls/client.pem"\nprivate_key="tls/key.pem"\n'); return loadClientConfig(path); };
        assert.equal(load('').timeoutSeconds, 15);
        assert.equal(load('').ca, join(directory, 'tls/ca.pem'));
        assert.equal(load('max_transfer_bytes_per_second=9007199254740993').maxTransferBytesPerSecond, 9007199254740993n);
        assert.equal(load('max_transfer_bytes_per_second=0').maxTransferBytesPerSecond, 0n);
        for (const fields of ['config_version=1', 'unexpected=true', 'max_transfer_bytes_per_second=9223372036854775808', 'max_transfer_bytes_per_second=1.0', 'timeout_seconds=nan', 'timeout_seconds=inf', 'timeout_seconds=true', 'timeout_seconds=1979-05-27', 'timeout_seconds=-1'])
            assert.throws(() => load(fields), { code: 'ERR_CEDEGRID_CONFIG' });
        // TOML 1.1-only escape must not slip through a 1.0 loader.
        assert.throws(() => load('endpoint="https://example.test\\e"'));
    }
    finally {
        rmSync(directory, { recursive: true, force: true });
    }
});
test('mixed require/import share the implementation and constructors', async () => {
    const cjs = require('../../dist/index.js');
    const esm = await (new Function('path', 'return import(path)') as (path: string) => Promise<any>)(pathToFileURL(join(__dirname, '../../dist/index.mjs')).href);
    assert.equal(cjs.Client, esm.Client);
    assert.equal(cjs.PublicationUncertain, esm.PublicationUncertain);
    assert(cjs.isCedeGridError(new esm.PublicationUncertain('id', {})));
    assert.equal(isCedeGridError({ code: 'ERR_CEDEGRID_REMOTE' }), true);
});
test('unsupported targets fail only native launcher selection', () => { assert.throws(() => nativePackage('freebsd' as any, 'x64'), { code: 'ERR_CEDEGRID_UNSUPPORTED_TARGET' }); assert.equal(typeof commandTask, 'function'); });
test('publication uncertainty preserves original operation ID and attempt', async () => {
    const context = new WorkerContext({ schema_version: 2n, namespace_id: 'ns', session_id: 's', task_id: 't', assignment_id: 'a', generation: 1n }, { async request() { throw new Error('lost ack'); } });
    await assert.rejects(context.complete({ value: 9007199254740993n }, [], { publicationId: 'recover' }), (error: unknown) => error instanceof PublicationUncertain && error.publicationId === 'recover' && error.identity.generation === 1n && error.digest === undefined && !!error.requestDigest && !!error.cause);
});
