import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createServer } from 'node:https';
import { mkdtempSync, readFileSync, writeFileSync, rmSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { setDefaultResultOrder } from 'node:dns';
import { Client, parseJson, stringifyJson } from 'cedegrid';
function pki(root: string): void {
    const run = (args: string[]) => execFileSync('openssl', args, { cwd: root, stdio: 'ignore' });
    for (const ca of ['ca', 'wrong'])
        run(['req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-keyout', `${ca}.key`, '-out', `${ca}.pem`, '-subj', `/CN=${ca}`, '-days', '1']);
    for (const name of ['server', 'operator', 'unauthorized']) {
        run(['req', '-newkey', 'rsa:2048', '-nodes', '-keyout', `${name}.key`, '-out', `${name}.csr`, '-subj', `/CN=${name}`]);
        writeFileSync(join(root, `${name}.ext`), name === 'server' ? 'subjectAltName=IP:127.0.0.1\nextendedKeyUsage=serverAuth\n' : 'extendedKeyUsage=clientAuth\n');
        run(['x509', '-req', '-in', `${name}.csr`, '-CA', 'ca.pem', '-CAkey', 'ca.key', '-CAcreateserial', '-out', `${name}.pem`, '-days', '1', '-extfile', `${name}.ext`]);
    }
}
test('installed SDK real mTLS, exact wire numbers, redirects, proxy bypass and absolute deadlines', async () => {
    const root = mkdtempSync(join(tmpdir(), 'cedegrid-tls-'));
    pki(root);
    setDefaultResultOrder('ipv4first');
    let requests = 0;
    const server = createServer({ key: readFileSync(join(root, 'server.key')), cert: readFileSync(join(root, 'server.pem')), ca: readFileSync(join(root, 'ca.pem')), requestCert: true, rejectUnauthorized: true }, (req, res) => {
        requests++;
        const cert = (req.socket as import('node:tls').TLSSocket).getPeerCertificate();
        if (cert.subject?.CN !== 'operator') {
            res.writeHead(403);
            res.end('{}');
            return;
        }
        const chunks: Buffer[] = [];
        req.on('data', (chunk: Buffer) => chunks.push(chunk));
        req.on('end', () => {
            const body = parseJson(Buffer.concat(chunks).toString()) as any;
            if (body.op === 'redirect') {
                res.writeHead(302, { location: '/v1/rpc' });
                res.end('{}');
                return;
            }
            if (body.op === 'slow') {
                res.writeHead(200, { 'content-type': 'application/json' });
                res.write('{"kind":"');
                const timer = setTimeout(() => res.end('ok"}'), 250);
                res.on('close', () => clearTimeout(timer));
                return;
            }
            if (body.op === 'large') {
                res.writeHead(200);
                res.end('x'.repeat(3 * 1024 * 1024 + 1));
                return;
            }
            res.writeHead(200, { 'content-type': 'application/json' });
            res.end(stringifyJson({ kind: 'echo', body }));
        });
    });
    server.on('tlsClientError', () => { });
    await new Promise<void>(resolve => server.listen(0, '127.0.0.1', resolve));
    const port = (server.address() as import('node:net').AddressInfo).port;
    const options = { ca: join(root, 'ca.pem'), certificate: join(root, 'operator.pem'), privateKey: join(root, 'operator.key'), maxTransferBytesPerSecond: 0n };
    const client = new Client(`https://127.0.0.1:${port}`, options);
    try {
        const previous = { HTTPS_PROXY: process.env.HTTPS_PROXY, HTTP_PROXY: process.env.HTTP_PROXY, ALL_PROXY: process.env.ALL_PROXY };
        Object.assign(process.env, { HTTPS_PROXY: 'http://127.0.0.1:1', HTTP_PROXY: 'http://127.0.0.1:1', ALL_PROXY: 'http://127.0.0.1:1' });
        try {
            const reply = await client.request('echo', { generation: 9007199254740993n, metadata: { integer: 18446744073709551615n, float: 1.0, zero: -0 } });
            assert.equal(reply.body.generation, 9007199254740993n);
            assert.equal(reply.body.metadata.integer, 18446744073709551615n);
            assert.equal(typeof reply.body.metadata.float, 'number');
            assert(Object.is(reply.body.metadata.zero, -0));
        }
        finally {
            for (const [k, v] of Object.entries(previous))
                v === undefined ? delete process.env[k] : process.env[k] = v;
        }
        const before = requests;
        await assert.rejects(client.request('redirect'), { code: 'ERR_CEDEGRID_HTTP' });
        assert.equal(requests, before + 1);
        await assert.rejects(new Client(`https://127.0.0.1:${port}`, { ...options, ca: join(root, 'wrong.pem') }).request('echo'), { code: 'ERR_CEDEGRID_TRANSPORT' });
        await assert.rejects(new Client(`https://localhost:${port}`, options).request('echo'), (error: any) => error.code === 'ERR_CEDEGRID_TRANSPORT' && error.cause?.code === 'ERR_TLS_CERT_ALTNAME_INVALID');
        await assert.rejects(new Client(`https://127.0.0.1:${port}`, { ...options, certificate: Buffer.alloc(0), privateKey: Buffer.alloc(0) }).request('echo'), { code: 'ERR_CEDEGRID_TRANSPORT' });
        await assert.rejects(new Client(`https://127.0.0.1:${port}`, { ...options, certificate: join(root, 'unauthorized.pem'), privateKey: join(root, 'unauthorized.key') }).request('echo'), { code: 'ERR_CEDEGRID_HTTP' });
        const slow = new Client(`https://127.0.0.1:${port}`, { ...options, timeoutSeconds: 0.05 });
        const started = performance.now();
        await assert.rejects(slow.request('slow'), { code: 'ERR_CEDEGRID_TIMEOUT' });
        assert(performance.now() - started < 200);
        await assert.rejects(client.request('large'), { code: 'ERR_CEDEGRID_RESPONSE_TOO_LARGE' });
        const paced = new Client(`https://127.0.0.1:${port}`, { ...options, timeoutSeconds: 0.02, maxTransferBytesPerSecond: 1n });
        const count = requests;
        await assert.rejects(paced.request('echo'), { code: 'ERR_CEDEGRID_TIMEOUT' });
        assert.equal(requests, count);
    }
    finally {
        await new Promise<void>(resolve => server.close(() => resolve()));
        rmSync(root, { recursive: true, force: true });
    }
});
