export class CedeGridError extends Error {
    readonly code: string;
    constructor(message: string, code = 'ERR_CEDEGRID_ERROR', options?: ErrorOptions) {
        super(message, options);
        this.name = new.target.name;
        this.code = code;
    }
}
export class ValidationError extends CedeGridError {
    constructor(message: string, options?: ErrorOptions) { super(message, 'ERR_CEDEGRID_VALIDATION', options); }
}
export class ConfigError extends CedeGridError {
    constructor(message: string, options?: ErrorOptions) { super(message, 'ERR_CEDEGRID_CONFIG', options); }
}
export class RemoteError extends CedeGridError {
    constructor(message: string, code = 'ERR_CEDEGRID_REMOTE', options?: ErrorOptions) { super(message, code, options); }
}
export class DeadlineExceeded extends CedeGridError {
    constructor(message = 'RPC end-to-end deadline exceeded', options?: ErrorOptions) { super(message, 'ERR_CEDEGRID_TIMEOUT', options); }
}
export class SpawnUncertain extends CedeGridError {
    constructor(readonly requestId: string, options?: ErrorOptions) { super(`spawn acknowledgement uncertain; retry request_id=${requestId}`, 'ERR_CEDEGRID_SPAWN_UNCERTAIN', options); }
}
export class PublicationUncertain extends CedeGridError {
    constructor(readonly publicationId: string, readonly identity: Record<string, unknown>, readonly digest?: string, options?: ErrorOptions, readonly requestDigest?: string) { super(`publication acknowledgement uncertain: ${publicationId}`, 'ERR_CEDEGRID_PUBLICATION_UNCERTAIN', options); }
}
export class DownloadUncertain extends CedeGridError {
    constructor(readonly destination: string, readonly operationId: string, readonly digest: string, options?: ErrorOptions) { super(`download publication durability uncertain: ${destination}`, 'ERR_CEDEGRID_DOWNLOAD_UNCERTAIN', options); }
}
export class DrainRequested extends CedeGridError {
    constructor() { super('supervisor requested cooperative drain', 'ERR_CEDEGRID_DRAIN_REQUESTED'); }
}
export function isCedeGridError(value: unknown): value is Error & {
    code: string;
} {
    return typeof value === 'object' && value !== null && 'code' in value && typeof value.code === 'string' && value.code.startsWith('ERR_CEDEGRID_');
}
