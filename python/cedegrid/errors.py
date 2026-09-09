"""Stable SDK error codes, independent of constructor identity."""
class CedeGridError(RuntimeError):
    code = "ERR_CEDEGRID_ERROR"
    def __init__(self, message, *, code=None, cause=None):
        super().__init__(message)
        self.code = code or type(self).code
        self.cause = cause
        if cause is not None:
            self.__cause__ = cause

class ValidationError(CedeGridError, ValueError):
    code = "ERR_CEDEGRID_VALIDATION"

class ConfigError(ValidationError):
    code = "ERR_CEDEGRID_CONFIG"

class RemoteError(CedeGridError):
    code = "ERR_CEDEGRID_REMOTE"

class DeadlineExceeded(CedeGridError, TimeoutError):
    code = "ERR_CEDEGRID_TIMEOUT"

class PublicationUncertain(CedeGridError):
    code = "ERR_CEDEGRID_PUBLICATION_UNCERTAIN"
    def __init__(self, publication_id, identity, *, digest=None, request_digest=None, cause=None):
        super().__init__(f"publication acknowledgement uncertain: {publication_id}", cause=cause)
        self.publication_id, self.identity, self.digest = publication_id, dict(identity), digest
        self.request_digest = request_digest

class SpawnUncertain(CedeGridError):
    code = "ERR_CEDEGRID_SPAWN_UNCERTAIN"
    def __init__(self, request_id, *, cause=None):
        super().__init__(f"spawn acknowledgement uncertain; retry request_id={request_id}", cause=cause)
        self.request_id = request_id

class DownloadUncertain(CedeGridError):
    code = "ERR_CEDEGRID_DOWNLOAD_UNCERTAIN"
    def __init__(self, destination, operation_id, digest, *, cause=None):
        super().__init__(f"download publication durability uncertain: {destination}", cause=cause)
        self.destination, self.operation_id, self.digest = str(destination), operation_id, digest

def is_cedegrid_error(value):
    return isinstance(getattr(value, "code", None), str) and value.code.startswith("ERR_CEDEGRID_")
