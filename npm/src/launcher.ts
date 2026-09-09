#!/usr/bin/env node
import { spawn } from 'node:child_process';
import { createRequire } from 'node:module';
import { CedeGridError } from './errors';
export function nativePackage(platform = process.platform, arch = process.arch): string {
    const target = `${platform}-${arch}`;
    const names: Record<string, string> = { 'linux-x64': 'cedegrid-linux-x64-gnu', 'linux-arm64': 'cedegrid-linux-arm64-gnu', 'darwin-x64': 'cedegrid-darwin-x64', 'darwin-arm64': 'cedegrid-darwin-arm64', 'win32-x64': 'cedegrid-win32-x64' };
    if (!names[target])
        throw new CedeGridError(`unsupported native target: ${target}`, 'ERR_CEDEGRID_UNSUPPORTED_TARGET');
    if (platform === 'linux' && !(process.report.getReport() as {
        header: {
            glibcVersionRuntime?: string;
        };
    }).header.glibcVersionRuntime)
        throw new CedeGridError('Linux CLI requires glibc', 'ERR_CEDEGRID_UNSUPPORTED_TARGET');
    return names[target];
}
export function launch(args = process.argv.slice(2)): void {
    const name = nativePackage();
    let binary: string;
    try {
        const resolve = createRequire(__filename);
        const manifest = resolve(`${name}/package.json`) as {
            version: string;
        };
        if (manifest.version !== '0.2.0')
            throw new Error(`native package version ${manifest.version} does not match 0.2.0`);
        binary = resolve.resolve(`${name}/bin/cedegrid${process.platform === 'win32' ? '.exe' : ''}`);
    }
    catch (cause) {
        throw new CedeGridError(`native package ${name}@0.2.0 is missing; reinstall with optional dependencies enabled`, 'ERR_CEDEGRID_MISSING_BINARY', { cause });
    }
    if (process.platform !== 'win32' && typeof process.execve === 'function') {
        process.execve(binary, [binary, ...args], Object.fromEntries(Object.entries(process.env).filter((entry): entry is [
            string,
            string
        ] => entry[1] !== undefined)));
        return;
    }
    // The native child verifies this parent PID, creates its own process group
    // within the same session, and takes foreground control for terminal stdin.
    // The launcher forwards directly delivered signals once.
    const child = spawn(binary, args, { stdio: 'inherit', detached: false, windowsHide: false, env: { ...process.env, CEDEGRID_LAUNCHER_PID: String(process.pid) } });
    const signals: NodeJS.Signals[] = process.platform === 'win32' ? ['SIGINT'] : ['SIGINT', 'SIGTERM', 'SIGHUP', 'SIGQUIT'];
    const handlers = new Map<NodeJS.Signals, () => void>();
    for (const signal of signals) {
        const handler = () => { if (child.pid && child.exitCode === null && child.signalCode === null)
            child.kill(signal); };
        handlers.set(signal, handler);
        process.on(signal, handler);
    }
    const cleanup = () => { for (const [signal, handler] of handlers)
        process.off(signal, handler); };
    child.on('error', cause => { cleanup(); process.stderr.write(`ERR_CEDEGRID_LAUNCH: ${cause.message}\n`); process.exitCode = 1; });
    child.on('exit', (code, signal) => { cleanup(); if (signal && process.platform !== 'win32') {
        process.kill(process.pid, signal);
    }
    else
        process.exitCode = code ?? 1; });
}
if (require.main === module) {
    try {
        launch();
    }
    catch (error) {
        const e = error as CedeGridError;
        process.stderr.write(`${e.code ?? 'ERR_CEDEGRID_LAUNCH'}: ${e.message}\n`);
        process.exitCode = 1;
    }
}
