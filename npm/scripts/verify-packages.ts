/** Validate unmodified candidates in an isolated registry with no uplinks. */
import { createRequire } from 'node:module';
import { createServer } from 'node:net';
import { spawn, spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { gunzipSync } from 'node:zlib';
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync, readdirSync, existsSync, copyFileSync, renameSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve, basename } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
const project = resolve(__dirname, '../..');
const args = process.argv.slice(2), candidate = resolve(args[0] ?? ''), seedDirectory = resolve(args[1] ?? '');
if (args.length !== 2 && !(args.length === 4 && args[2] === '--keep-local'))
    throw new Error('usage: node build/scripts/verify-packages.js CANDIDATE_DIR DEPENDENCY_TARBALL_DIR [--keep-local DIRECTORY]');
const keepLocal = args[3] ? resolve(args[3]) : undefined;
const candidateManifest = join(candidate, existsSync(join(candidate, 'manifest.json')) ? 'manifest.json' : 'manifest.partial.json');
if (keepLocal && existsSync(keepLocal))
    throw new Error('the retained install directory must not exist');
const root = mkdtempSync(join(tmpdir(), 'cedegrid-package-check-'));
const sha = (data: Buffer, algorithm = 'sha256') => createHash(algorithm).update(data).digest('hex');
function packageManifest(archive: Buffer): any {
    const tar = gunzipSync(archive);
    for (let offset = 0; offset + 512 <= tar.length;) {
        const header = tar.subarray(offset, offset + 512), name = header.subarray(0, 100).toString().replace(/\0.*$/s, ''), size = parseInt(header.subarray(124, 136).toString().replace(/\0.*$/s, '').trim() || '0', 8);
        if (name === 'package/package.json')
            return JSON.parse(tar.subarray(offset + 512, offset + 512 + size).toString());
        offset += 512 + Math.ceil(size / 512) * 512;
    }
    throw new Error('package/package.json missing');
}
function run(command: string, commandArgs: string[], cwd: string, extraEnv: NodeJS.ProcessEnv = {}): string {
    const result = spawnSync(command, commandArgs, { cwd, env: { ...process.env, ...extraEnv }, encoding: 'utf8', maxBuffer: 16 * 1024 * 1024 });
    if (result.status !== 0)
        throw new Error(`${command} failed (${result.status}): ${result.stderr}\n${result.stdout}`);
    return result.stdout;
}
async function main(): Promise<void> {
    const reserve = createServer();
    await new Promise<void>(done => reserve.listen(0, '127.0.0.1', done));
    const port = (reserve.address() as import('node:net').AddressInfo).port;
    await new Promise<void>(done => reserve.close(() => done()));
    const registry = `http://127.0.0.1:${port}`, configuration = join(root, 'verdaccio.yaml');
    writeFileSync(configuration, `storage: ${JSON.stringify(join(root, 'storage'))}\nauth:\n  htpasswd:\n    file: ${JSON.stringify(join(root, 'htpasswd'))}\n    max_users: -1\nuplinks: {}\npackages:\n  '@*/*':\n    access: $all\n    publish: $all\n    unpublish: $all\n  '**':\n    access: $all\n    publish: $all\n    unpublish: $all\nlog: {type: stdout, format: json, level: warn}\n`);
    const server = spawn(process.execPath, [join(project, 'node_modules/verdaccio/bin/verdaccio'), '-c', configuration, '-l', `127.0.0.1:${port}`], { stdio: ['ignore', 'pipe', 'pipe'] });
    let logs = '';
    server.stdout.on('data', data => logs += data);
    server.stderr.on('data', data => logs += data);
    try {
        let ready = false;
        for (let i = 0; i < 200; i++) {
            if (server.exitCode !== null)
                throw new Error(`registry exited: ${logs}`);
            try {
                const reply = await fetch(`${registry}/-/ping`);
                if (reply.ok) {
                    ready = true;
                    break;
                }
            }
            catch { }
            await delay(50);
        }
        if (!ready)
            throw new Error(`registry did not start: ${logs}`);
        const mainTarball = join(candidate, 'npm', 'cedegrid-0.2.0.tgz');
        const originalHash = sha(readFileSync(mainTarball));
        const seeds = [...readdirSync(seedDirectory).filter(name => name.endsWith('.tgz')).map(name => join(seedDirectory, name)), ...readdirSync(join(candidate, 'npm')).filter(name => name.endsWith('.tgz') && name !== 'cedegrid-0.2.0.tgz').map(name => join(candidate, 'npm', name))];
        const lock = JSON.parse(readFileSync(join(project, 'package-lock.json'), 'utf8'));
        const seedRecords = [];
        for (const path of seeds) {
            const archive = readFileSync(path), manifest = packageManifest(archive), integrity = 'sha512-' + createHash('sha512').update(archive).digest('base64');
            const locked = lock.packages[`node_modules/${manifest.name}`];
            if (manifest.name === 'lossless-json' || manifest.name === '@ltd/j-toml') {
                if (!locked || locked.version !== manifest.version || locked.integrity !== integrity)
                    throw new Error(`original locked dependency integrity mismatch: ${manifest.name}`);
            }
            const filename = basename(path), attachment = { content_type: 'application/octet-stream', data: archive.toString('base64'), length: archive.length };
            const document = { _id: manifest.name, name: manifest.name, description: manifest.description ?? '', 'dist-tags': { latest: manifest.version }, versions: { [manifest.version]: { ...manifest, dist: { tarball: `${registry}/${manifest.name}/-/${filename}`, shasum: sha(archive, 'sha1'), integrity } } }, _attachments: { [filename]: attachment } };
            const response = await fetch(`${registry}/${encodeURIComponent(manifest.name)}`, { method: 'PUT', headers: { 'content-type': 'application/json' }, body: JSON.stringify(document) });
            if (!response.ok)
                throw new Error(`seed rejected ${manifest.name}: ${response.status} ${await response.text()}`);
            seedRecords.push({ name: manifest.name, version: manifest.version, sha256: sha(archive) });
        }
        const installations = [];
        for (const mode of ['local', 'global', 'omit-optional']) {
            const directory = join(root, mode);
            mkdirSync(directory);
            const prefix = join(directory, 'prefix'), cache = join(directory, 'cache'), userconfig = join(directory, 'npmrc'), globalconfig = join(directory, 'global-npmrc');
            writeFileSync(userconfig, `registry=${registry}\naudit=false\nfund=false\nignore-scripts=true\n`);
            writeFileSync(globalconfig, '');
            if (mode !== 'global')
                writeFileSync(join(directory, 'package.json'), '{"name":"cedegrid-release-test","version":"1.0.0","private":true}');
            const installArgs = ['install', mainTarball, '--ignore-scripts', '--no-audit', '--no-fund', '--registry', registry, '--cache', cache, '--userconfig', userconfig, '--globalconfig', globalconfig];
            if (mode === 'global')
                installArgs.push('--global', '--prefix', prefix);
            if (mode === 'omit-optional')
                installArgs.push('--omit=optional');
            // npm runs synchronously while the registry is a separate process.
            const installOutput = run('npm', installArgs, directory);
            console.log(mode + ': ' + installOutput.trim());
            const moduleBase = mode === 'global' ? join(prefix, process.platform === 'win32' ? 'node_modules' : 'lib/node_modules') : join(directory, 'node_modules');
            const packageDirectory = join(moduleBase, 'cedegrid');
            const installed = JSON.parse(readFileSync(join(packageDirectory, 'package.json'), 'utf8'));
            if (installed.version !== '0.2.0')
                throw new Error('installed main version mismatch');
            run(process.execPath, ['-e', `const s=require(${JSON.stringify(packageDirectory)}); if(s.parseJson('9007199254740993')!==9007199254740993n)throw Error('integer loss'); import(${JSON.stringify('file://' + join(packageDirectory, 'dist/index.mjs'))}).then(m=>{if(m.Client!==s.Client)throw Error('constructor mismatch')})`], directory);
            const target = process.platform === 'linux' ? `linux-${process.arch}-gnu` : `${process.platform}-${process.arch}`, nativeName = 'cedegrid-' + target, nativePath = (() => { try {
                return createRequire(join(packageDirectory, 'package.json')).resolve(nativeName + '/bin/' + (process.platform === 'win32' ? 'cedegrid.exe' : 'cedegrid'));
            }
            catch {
                return join(moduleBase, nativeName, 'bin', process.platform === 'win32' ? 'cedegrid.exe' : 'cedegrid');
            } })();
            const cli = spawnSync(process.execPath, [join(packageDirectory, 'dist/launcher.js'), '--version'], { cwd: directory, encoding: 'utf8' });
            if (mode === 'omit-optional') {
                if (existsSync(nativePath) || cli.status === 0 || !cli.stderr.includes('ERR_CEDEGRID_MISSING_BINARY'))
                    throw new Error('omit-optional behavior failed');
            }
            else if (existsSync(nativePath)) {
                const expected = JSON.parse(readFileSync(candidateManifest, 'utf8')).native_inputs[target]?.sha256;
                if (!expected || sha(readFileSync(nativePath)) !== expected)
                    throw new Error('installed native binary hash mismatch');
                if (cli.status !== 0 || !cli.stdout.includes('0.2.0'))
                    throw new Error(`native launcher failed: ${cli.stderr}`);
            }
            else if (cli.status === 0 || !cli.stderr.includes('ERR_CEDEGRID_MISSING_BINARY'))
                throw new Error('missing native behavior failed: ' + JSON.stringify({ mode, nativePath, status: cli.status, stdout: cli.stdout, stderr: cli.stderr, installed: readdirSync(moduleBase) }));
            if (mode === 'local') {
                const test = join(directory, 'transport.test.cjs');
                copyFileSync(join(project, 'build/tests/transport.test.js'), test);
                run(process.execPath, ['--test', test], directory);
                const consumer = join(directory, 'consumer.mts');
                writeFileSync(consumer, "import {Client,commandTask,parseJson,PublicationUncertain} from 'cedegrid';\nconst c: Client | null = null; commandTask('t',['worker'],'/work',{cpuMillicores:1n}); parseJson('1'); new PublicationUncertain('id',{});\n");
                run(process.execPath, [join(project, 'node_modules/typescript/bin/tsc'), '--noEmit', '--module', 'NodeNext', '--moduleResolution', 'NodeNext', '--target', 'ES2022', '--skipLibCheck', consumer], directory);
            }
            installations.push({ mode, main_sha256: originalHash, native_present: existsSync(nativePath), status: 'PASS' });
        }
        if (sha(readFileSync(mainTarball)) !== originalHash)
            throw new Error('candidate tarball changed during verification');
        const evidence = { schema_version: 1, status: 'PASS', registry_uplinks: {}, main_sha256: originalHash, seeds: seedRecords, installations, node: process.version, platform: process.platform, architecture: process.arch, registry_publication: 'NOT_RUN' };
        writeFileSync(join(candidate, `npm-install-evidence-node${process.versions.node.split('.')[0]}.json`), JSON.stringify(evidence, null, 2) + '\n');
        if (keepLocal)
            renameSync(join(root, 'local'), keepLocal);
        console.log(JSON.stringify(evidence, null, 2));
    }
    finally {
        server.kill('SIGTERM');
        await new Promise<void>(done => { if (server.exitCode !== null)
            done();
        else {
            server.once('exit', () => done());
            setTimeout(() => { server.kill('SIGKILL'); done(); }, 5000).unref();
        } });
        rmSync(root, { recursive: true, force: true });
    }
}
main().catch(error => { console.error(error); process.exitCode = 1; });
