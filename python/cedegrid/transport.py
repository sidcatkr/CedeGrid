"""No-proxy, no-redirect mTLS requests with an absolute monotonic deadline."""
import http.client
import socket
import ssl
import threading
import time
from queue import Queue, Empty
from urllib.parse import urlsplit
from .errors import DeadlineExceeded, RemoteError
from .codec import parse_json, stringify_json, protocol_value

def remaining(deadline):
    value = deadline - time.monotonic()
    if value <= 0:
        raise DeadlineExceeded("RPC end-to-end deadline exceeded")
    return value

def resolve(host, port, deadline):
    # OS DNS resolution is not interruptible. Only this credential-free lookup
    # may outlive a deadline; all connection and transmission stay in the caller.
    result = Queue(1)
    def run():
        try: result.put((socket.getaddrinfo(host, port, type=socket.SOCK_STREAM), None))
        except OSError as error: result.put((None, error))
    threading.Thread(target=run, daemon=True, name="cedegrid-dns").start()
    try: addresses, error = result.get(timeout=remaining(deadline))
    except Empty as error: raise DeadlineExceeded("DNS deadline exceeded", cause=error) from error
    if error: raise error
    return addresses

def exchange(client, op, payload, deadline):
    data = stringify_json(protocol_value({"op": op, **payload})).encode("utf-8")
    if len(data) > 3 * 1024 * 1024:
        raise RemoteError("RPC request exceeds 3 MiB", code="ERR_CEDEGRID_REQUEST_TOO_LARGE")
    reserved = 2 * int(payload.get("max_bytes", 0)) + 512 if op == "read_artifact" else 0
    client.pacer.account(len(data) + reserved, deadline=deadline)
    parsed = urlsplit(client.url)
    raw_socket = tls_socket = response = None
    try:
        last_error = None
        for family, socktype, proto, _, address in resolve(parsed.hostname, parsed.port or 443, deadline):
            try:
                raw_socket = socket.socket(family, socktype, proto)
                raw_socket.settimeout(remaining(deadline))
                raw_socket.connect(address)
                break
            except OSError as error:
                last_error = error
                raw_socket.close()
                raw_socket = None
        if raw_socket is None: raise last_error or OSError("DNS returned no addresses")
        raw_socket.settimeout(remaining(deadline))
        tls_socket = client.context.wrap_socket(raw_socket, server_hostname=parsed.hostname)
        tls_socket.settimeout(remaining(deadline))
        host = parsed.hostname.encode("idna").decode("ascii")
        host = f"[{host}]" if ":" in host else host
        if parsed.port is not None: host += f":{parsed.port}"
        headers = (f"POST /v1/rpc HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {len(data)}\r\nConnection: close\r\n\r\n").encode("ascii")
        tls_socket.sendall(headers + data)
        tls_socket.settimeout(remaining(deadline))
        response = http.client.HTTPResponse(tls_socket)
        response.begin()
        limit = (8 if op in ("status", "status_page") else 3) * 1024 * 1024
        body = bytearray()
        while True:
            tls_socket.settimeout(remaining(deadline))
            part = response.read1(min(65536, limit + 1 - len(body)))
            if not part: break
            body.extend(part)
            if len(body) > limit:
                raise RemoteError("RPC response exceeds the client bound", code="ERR_CEDEGRID_RESPONSE_TOO_LARGE")
        client.pacer.account(max(0, len(body) - reserved), deadline=deadline)
        remaining(deadline)
        if response.status != 200:
            raise RemoteError(f"RPC HTTP failure: {response.status}", code="ERR_CEDEGRID_HTTP")
        value = parse_json(body)
        remaining(deadline)
        if type(value) is not dict:
            raise RemoteError("invalid RPC response", code="ERR_CEDEGRID_PROTOCOL")
        if value.get("kind") == "error":
            code = value.get("code")
            if type(code) is not str or not code.startswith("ERR_CEDEGRID_"): code = "ERR_CEDEGRID_REMOTE"
            raise RemoteError(value.get("message", "RPC rejected"), code=code)
        return value
    except (socket.timeout, TimeoutError) as error:
        if isinstance(error, DeadlineExceeded): raise
        raise DeadlineExceeded("RPC end-to-end deadline exceeded", cause=error) from error
    except (OSError, http.client.HTTPException) as error:
        raise RemoteError("RPC transport failed", code="ERR_CEDEGRID_TRANSPORT", cause=error) from error
    finally:
        if response is not None: response.close()
        if tls_socket is not None: tls_socket.close()
        elif raw_socket is not None: raw_socket.close()
