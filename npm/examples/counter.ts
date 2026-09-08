import { WorkerContext, DrainRequested } from 'cedegrid';
async function main(): Promise<void> {
    const context = WorkerContext.fromEnv();
    let cursor = 0n;
    try {
        for (; cursor < 1000n; cursor++) {
            context.safePoint();
            if (cursor % 100n === 0n)
                await context.checkpoint({ cursor });
        }
        await context.complete({ cursor });
    }
    catch (error) {
        if (!(error instanceof DrainRequested))
            throw error;
        await context.checkpoint({ cursor });
    }
}
main().catch(error => { console.error(error); process.exitCode = 1; });
