# Contract: names, fields, refusals and markers

This release has no 003 CLI. 003 adds the `run` flags and the `ps` and `top` markers below,
under the same names and through the parity test.

006 renamed unprotected Aggressive to unprotected, which also applies to Performance; the names
below are 006's. 006's contract cases RO-01 to RO-08 in `tests/contracts/cedegrid-0.2.json` pin
them, together with admit immediately.

## Configuration (`config_version = 1`)

| Key | Values | Rule |
|---|---|---|
| `contention.default_mode`, `contention.<resource>.mode` | adds `aggressive` | needs `aggressive_mode` in `contention.acknowledge_caution` |
| `contention.max_mode` | adds `aggressive` | only when set explicitly, with `aggressive_mode`; the derived cap stops at `performance` |
| `contention.acknowledge_caution` | adds `aggressive_mode` | unknown entries still fail to load |
| any key that would enable unprotected (`unprotected`, `allow_unprotected`, anywhere in the file) | — | fails to load: `ERR_CEDEGRID_UNPROTECTED_NOT_STORABLE: <key> cannot be stored; unprotected is requested per submission only` |

## Setup (001's `cedegrid setup`)

| Step | Rule |
|---|---|
| raise the node cap to Aggressive | its own confirmed step interactively; non-interactively `--max-mode aggressive --acknowledge aggressive_mode` |
| turn the allowance on | interactive only: the numbers screen, `Allow unprotected on this host? [y/n]` with no default, then `Type "slow other users' compute on this host" to confirm:`. Anything else cancels with one line: `Cancelled. The allowance stays off.`, or `Cancelled: the phrase did not match. The allowance stays off.` Non-interactive: `ERR_CEDEGRID_ALLOWANCE_INTERACTIVE_ONLY` |
| turn the allowance off | no confirmation, interactive or `cedegrid setup --disallow-unprotected` |

The numbers screen shows the number of other active principals and their current CPU use, and
says that where they contend the kernel splits CPU time, so their work can slow down until the
run ends. It gives no slowdown figure: that depends on the run's threads and the host's cgroup
layout. Memory floors and OOM handling do not change.

## SDKs (Python and TypeScript, the same names)

| Field | Type | Meaning |
|---|---|---|
| per-resource mode | `"aggressive"` | protected Aggressive, within the node cap |
| `unprotected` | bool, default false | request the grant for this submission (Performance or Aggressive work) |
| `confirm_unprotected` | bool, default false | the second explicit confirmation |

A parity test fails when the two SDKs and the wire contract differ (FR-019). 003's CLI joins it.

## Wire (protocol v1, job and assignment)

- Job modes accept `aggressive`: contract case JC-05 becomes accepted. A job carries no
  acknowledgement; the node cap decides the effective mode, with `mode_capped` below Aggressive.
- Job field `unprotected: { "confirmed": true, "requested_unix_ms": <u64> }`; absent
  means protected. The SDK sets it only when both SDK fields are true. The launching agent
  verifies it before Authorized; an older agent rejects the job.
- New contract cases: the grant without `confirmed` (rejected at submission), the grant on a node
  whose cap stops at Performance (runs protected at the cap with `mode_capped`, refused as a
  grant), the grant with the allowance off (refused at launch).

## Refusals at launch (one line each; every missing condition is named)

- `ERR_CEDEGRID_UNPROTECTED_CONFIRMATION: confirm_unprotected is missing`
- `ERR_CEDEGRID_UNPROTECTED_NOT_ALLOWED: the host's "allow unprotected" setting is off`
- `ERR_CEDEGRID_UNPROTECTED_ALLOWANCE_UNREADABLE: <reason>; treated as off`
- `ERR_CEDEGRID_UNPROTECTED_POLICY: this host's System mode policy does not permit unprotected (modes.unprotected)`

006 removed the Aggressive-cap refusal: unprotected applies to Performance work as well.

## Administrator policy (System mode, `/etc/cedegrid/policy.toml`)

```toml
[modes]
unprotected = false             # default; true lets members request the grant
unprotected_cpu_weight = 400    # 006: the account slice's CPUWeight while a raise is in force
```

## Markers

| Output | Protected Aggressive | Unprotected |
|---|---|---|
| setup, `doctor`, `status`, `observe` (and `ps`, `top` with 003) | red `[Aggressive]` | red `[Aggressive]` and red `UNPROTECTED` |
| JSON | `"mode": "aggressive"` | also `"unprotected": true` and the grant |
| logs | mode in each decision | one warning line at start: `warning: this run is UNPROTECTED: other users' compute may slow down until it ends` |

With `NO_COLOR`, the same text without color.
