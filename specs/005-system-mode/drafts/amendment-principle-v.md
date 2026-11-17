# Draft: constitution amendment for feature 005, System mode

**Status**: draft for the user's approval. Nothing here is applied. The constitution stays at
7.0.0 until the user approves this text and a reviewed pull request carries it. Approving it lets
feature 005's implementation start (tasks.md, the gate). The text itself lands in the
constitution with that implementation's pull request (tasks.md T069), with `docs/guarantees.md`'s
System mode section and the tests, as Governance requires for a relaxed principle.

**Version**: 7.0.0 → 8.0.0. Principle V is redefined (a root process becomes allowed), so the
change is MAJOR under the Governance section.

**Reason**: the user decided on 2026-10-27 to 2026-10-29, and again on 2026-11-14, that System
mode comes before the 001 merge: an administrator-installed root broker that gives every member
of the `cedegrid` group the Full level on hosts that give users no delegated cgroup, without
making the group equivalent to root, and that protects own in-share work in an OOM others caused.
Today's Principle V ("CedeGrid MUST run without privileges") forbids any root process, so the
feature cannot be built inside the present text (Development Workflow: "A proposed relaxation is
raised as an amendment").

## What needs the user's approval

| Part | Decision | Recommended | If not approved |
|---|---|---|---|
| Principle V, redefined | D4 | approve | nothing in feature 005 is built |
| Principle II, one OOM clause | N5 | A: approve | the broker only marks over-share tasks at 1000 (as N1 C); in-share own work keeps plain Linux's OOM risk |
| Principle I, one verified-set clause | N6 | A: approve | the broker acts only on processes that are also on the agent's verified list; a job's processes outside the supervisor's tree keep the default score and are never marked |
| Hidden dispatch constraint, two names added | with D4 | approve | none; it follows D4 |

The brief for this work said Principles I and II stay as they are. Two conflicts were found while
planning ([research.md](../research.md) R15), so each gets the smallest clause that points to
Principle V, and each is a separate choice.

## The diff

### Principle V (D4)

```diff
-### V. Rootless and User Space
+### V. Rootless by Default, One Root Broker in System Mode
 
-- CedeGrid MUST run without privileges. It uses no setuid binaries, added capabilities, kernel
-  modules or sysctl changes.
-- cgroup control MUST stay inside an explicitly delegated cgroup v2 subtree. Every write MUST be
-  read back. Nonempty cgroups are kept for reconciliation.
+- CedeGrid MUST run without privileges by default. The default install uses no setuid binaries,
+  added capabilities, kernel modules or sysctl changes, and every guarantee MUST hold with
+  ordinary user rights.
+- **System mode, the one privileged part.** Where an administrator installs the system package,
+  one broker process MAY run as root. Root is limited to that broker: the agent, the supervisors
+  and all user work run with the user's rights. The broker:
+  - judges a caller only by the socket's peer credentials, serves only members of the
+    `cedegrid` group, and takes no identity, executable, unit name, cgroup path, mount, device
+    or capability from a request;
+  - starts and stops only the units it made for that caller, built from the caller's user ID and
+    the administrator policy, each running a fixed CedeGrid subcommand at a root-owned path; it
+    never runs user input;
+  - moves only the caller's own verified work between the caller's own units, held still from
+    the check to the move;
+  - writes OOM score adjustments only for processes in its verified set (Principle I), and
+    lowers one only for the part of its user's memory within that user's fair share, which it
+    reads from kernel counters itself; it never writes −1000;
+  - writes no other file than those scores, the cgroup files of those moves and, where systemd
+    cannot set it, the idle slice's `cpu.idle`; it changes no PAM, login, sysctl or kernel module
+    setting, and purging the package leaves the host as before the install;
+  - is confined as far as the host's systemd allows, and its code gets a security review before
+    each release.
+
+  Membership of the `cedegrid` group MUST NOT give more than these rules allow.
+- cgroup control MUST stay inside an explicitly delegated cgroup v2 subtree: one the host
+  delegates to the user, or, in System mode, the units the broker had systemd make for the user.
+  Every write MUST be read back, the broker's included. Nonempty cgroups are kept for
+  reconciliation.
 - Rootless execution supports single-process commands, SDK-mediated managed children and, at
   the Tracked control level, descendants that stay in the run's subreaper tree, each verified
   under Principle I. Work started outside the tree, such as through another daemon, is not
   tracked and MUST NOT be presented as contained. Tracked is not containment: a run has no RAM
   cap, and no CPU limit below one core across cgroups.
 - CedeGrid MUST NOT be described as a sandbox for hostile code or as a hard VRAM partition.
 
-Rationale: every guarantee has to hold with ordinary user rights and without host-level
-configuration.
+Rationale: every guarantee of the default install has to hold with ordinary user rights and
+without host-level configuration. System mode is an administrator's explicit choice to give the
+Full level where the host gives users no delegation. Root stays in one small, reviewed process
+that acts only for the caller and only on the caller's own CedeGrid work, so the group never
+becomes equivalent to root.
```

### Principle II (N5), on the 7.0.0 text

```diff
 - **OOM order by fault.** Own work within its memory fair share MUST stay at the OOM score
   adjustment it launched with, the same baseline as every other process, and MUST NOT be set
-  below it. The own tasks that make up own memory above its fair share MUST be marked as
+  below it, except by the System mode broker as Principle V allows. The own tasks that make up
+  own memory above its fair share MUST be marked as
   preferred OOM victims, in the ladder's victim order and for every process in each task's
   family, until own memory is back within its share, and then return to their baseline. So an
   OOM that own work causes takes own excess first. Without privileges, in-share own work is
   protected only up to everyone else's baseline: the kernel can still take it when it is the
   largest process. An explicit opt-in MAY mark every own process as a preferred victim; that
   risk falls only on own work.
+- **System mode's OOM exception.** Where the broker lowers in-share own work's score, the kernel
+  MAY kill another workload's process in its place. This is the one OOM exception to the rule
+  that CedeGrid MUST NOT drive another workload into OOM kills. It applies only to the part of
+  own memory within its fair share, and only while the lowering is in force; when it is in force
+  is the feature's rule (005 FR-024, decision N1).
```

**If item 057 Part B is approved too**, the same two changes apply to 057's text
([057-amendment-draft.md](../../001-contention-modes/drafts/057-amendment-draft.md)): its
"OOM order" bullet gets the same clause after "MUST NOT be set below it", and the exception
bullet says "the rule that CedeGrid MUST NOT drive another workload into OOM kills beyond what
the baseline does". In Performance and Aggressive, where 057 marks no own task, the broker then
marks none either (005 D2, case A).

### Principle I (N6)

```diff
   Such a descendant carries the run's boot ID, assignment and generation, and CedeGrid acts on
   it only through that pidfd, or through a thread-ID call under the next rule, so a reused PID is
   never acted on. A process that leaves the tree, or whose chain cannot be confirmed, is never
   acted on.
+- **System mode units.** For the broker's score writes and moves (Principle V), a process is
+  also verified when it is inside a unit the broker made, runs with that unit's user and group
+  IDs, and is held by a pidfd checked against its start time. Such a unit's cgroup gains a
+  process only by a fork inside it or by the broker's own move of verified work, so no other
+  workload's process can be there. This is the one exception to the parent-chain rule above, and
+  it reaches no process outside those units.
```

### Execution and Export Constraints (with D4)

```diff
-- **Hidden dispatch.** The `__worker-gate` and `__assignment-supervisor` dispatch MUST run before
-  argument parsing, before any Tokio runtime or thread starts, and before any terminal handoff.
+- **Hidden dispatch.** The `__worker-gate` and `__assignment-supervisor` dispatch, and in System
+  mode the broker's and the idle executor's, MUST run before argument parsing, before any Tokio
+  runtime or thread starts, and before any terminal handoff.
```

### Version line

```diff
-**Version**: 7.0.0 | **Ratified**: 2026-10-24 | **Last Amended**: 2026-10-29
+**Version**: 8.0.0 | **Ratified**: 2026-10-24 | **Last Amended**: <the merge date>
```

## How it fits with item 057's draft

Item 057 Part B ([057-amendment-draft.md](../../001-contention-modes/drafts/057-amendment-draft.md))
redefines Principle II. Both drafts touch Principle II's OOM bullet: 057 rewrites it, and this
draft adds one clause to it (N5). The clause reads the same on either text, as shown above.

- Both approved before either lands: one revision, 8.0.0, with one Sync Impact Report, and the
  clause applied to 057's text.
- One approved first: it is 8.0.0, and the other is 9.0.0. If 057 lands second, its rewrite of
  the OOM bullet keeps the clause.

## Check against the other principles

- **I (untouchable processes)**: under N6 A, one bullet adds the System mode units to the
  verified set for the broker only; every other rule of Principle I holds, and "Every actuator
  MUST have tests" covers the broker (tasks.md T044: a process outside the units keeps its
  score). Under N6 B, Principle I is unchanged.
- **II (no harm)**: under N5 A, one clause and one exception bullet, bounded by the fair share.
  System mode adds no give-back of its own (005 FR-036).
- **III (unknown)**: unchanged. Unreadable memory lowers no score, and lowered scores return to
  the default after a bounded window (005 FR-026).
- **IV (pure policy)**: unchanged. `policy.rs` does not change; the broker's score function is
  pure and tested on its own.
- **VI (tests first)**: the security suite and the score tests come first, in a container with
  systemd as PID 1 (005 tasks, Phases 2 to 7).
- **VII (compatibility)**: no wire, SDK or worker-context change. The broker's protocol is new,
  local and versioned.
- **VIII (docs)**: `docs/guarantees.md` gets a System mode section in the same pull request
  (tasks.md T062, T069).
- **Governance**: MAJOR. The pull request states the version change and this reason, updates
  `docs/guarantees.md` and the affected tests, and needs the maintainer's explicit approval.
