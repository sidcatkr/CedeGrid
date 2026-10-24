# Plan notes: agent integration

Input for `/speckit-plan`. These are candidates and current-code facts, not decisions. Line
numbers refer to `dev` at `6be4cea`.

## Current code facts

- **CLI.** It is clap-derived in `src/main.rs` (`Command` enum). Commands include `submit`,
  `status`, `pool`, `cancel`, `resume`, `drain`, `config-example`, `config migrate`, `validate`,
  `doctor`, `observe`, `replay`, `supervise`, `backup`, `restore`, `state upgrade`,
  `coordinator`, `agent` and `rpc`.
  - The hidden `__worker-gate` / `__assignment-supervisor` argv dispatch runs before clap.
  - Windows accepts client commands only (`src/main.rs:296-316`).
- **Raw RPC.** `rpc` sends arbitrary protocol requests. FR-010 excludes it from MCP.
- **Error codes.** Many refusals already use `ERR_CEDEGRID_*` prefixes. Coordinator and state
  refusals mostly do not, and tests often match prose (audit M39), so FR-024 needs a code audit.
- **Output.** Several commands already print JSON (`observe`, `doctor`, `executions`, `status`).
  Others print prose. No published schemas exist.
- **Packages.**
  - npm: `npm/src/launcher.ts` resolves the exact-version optional platform package and execs
    the native binary.
  - Python: `python/` is a pure SDK with no native binary.
- **No `AGENTS.md`.** `CLAUDE.md` exists and is not in the export allowlist.

## Mechanism candidates

| Topic | Candidate | Notes |
|---|---|---|
| Transport | MCP over stdio (JSON-RPC 2.0, newline-delimited) | Matches FR-017 (local only). Streamable HTTP is out of scope; it would need authentication and a listener. |
| Implementation | Hand-written JSON-RPC over the existing `serde_json` and `tokio` | No new dependency. The MCP surface needed is small: initialize, tools/list, tools/call, notifications. |
| Implementation, alternative | A Rust MCP crate (for example `rmcp`) | A new dependency: needs the written justification the constitution requires, plus MSRV 1.88 compatibility and a check of its transitive dependencies. |
| Tool annotations | `readOnlyHint`, `destructiveHint`, `idempotentHint`, `openWorldHint=false` | Set per tool. Disabled tools are not listed (FR-009). |
| Untrusted output | Wrap workload-originated fields in a labelled envelope, for example `{"untrusted": true, "content": ...}`, and say so in tool descriptions | FR-014. |
| Command reference | Generate from clap at build or test time (`clap::Command` introspection) into `docs/cli-reference.json` (and Markdown) | A CI test regenerates and diffs it (FR-003, SC-005). |
| Agent guide | `docs/agents.md`, served by `cedegrid help agents` (embedded with `include_str!`) and shipped in the npm and Python packages | New shipped files need `ALLOWLIST` plus Cargo `include` entries (exporter rules). |
| Repository entry points | `AGENTS.md` holds the content and `CLAUDE.md` imports it (`@AGENTS.md`), or the reverse | FR-005. Today `CLAUDE.md` holds it. |
| Start commands | `cedegrid mcp` (native); `npx cedegrid mcp` (npm launcher passes through); a `cedegrid-mcp` console script in the Python package that locates `cedegrid` on PATH or a configured path, checks the version, and execs it | FR-019, FR-020. |
| Tool policy | `[mcp] enabled_tools = ["setup", "submit"]` in config, or `cedegrid mcp --enable submit` | The client cannot change it (FR-008). |
| Audit log | Append-only JSON lines under `state_dir/mcp-audit/`, with secrets redacted, rotated by size | FR-013. Document retention. |
| Concurrency | Serialize state-changing calls; read-only calls run concurrently with a per-call timeout | Edge cases. |

## Open questions for the plan

- Exact tool list and names, mapped one-to-one to CLI operations, and which CLI commands need
  structured output added first.
- Whether "explain the current yield decision" depends on the per-resource status and reason
  codes from 001 (FR-038/FR-039). It probably should, so plan 002 after those parts of 001.
- How to test SC-001 (reference agent quickstart) reproducibly in CI, or as a release gate.
