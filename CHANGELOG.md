# Changelog

## Unreleased

### Changed

- **Operator certificates can no longer act for a node's assignment.** Assignment-scoped
  requests now require the certificate of the node that owns the assignment. Before, an
  operator certificate could also send them. The affected requests are `prepared`, `renew`,
  `complete`, `publish_checkpoint`, `fail`, `begin_upload`, `upload_chunk`, `commit_upload` and
  `abort_upload`. Operator requests such as `submit`, `status`, `cancel`, `retry`, `get_result`
  and artifact reads are unchanged.
  - **Migration:** a tool that sent those requests with an operator certificate must now use
    the owning node's certificate.

### Fixed

- Internal gates and supervisors no longer fail with "launcher parent identity mismatch" when
  CedeGrid is started through the npm launcher on Node versions without `process.execve`.
- A supervisor no longer aborts, and leaves its workload unsupervised, when its diagnostic
  stderr pipe has no reader.
- The coordinator retries transient connection-accept errors instead of stopping.
- Lease expiry is atomic. A failed requeue no longer leaves a task assigned to an expired
  lease.
- `tools/release_manifest.py` runs on Python 3.10.
