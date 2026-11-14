# Draft: constitution amendment for item 057 Part B

**Status**: draft for the user's approval. Nothing here is applied. The constitution stays at
7.0.0 until the user approves a variant of every open point below and a reviewed pull request
carries the change.

**Version**: 7.0.0 → 8.0.0. Principle II is redefined (what counts as harm, and what Performance
gives back), so the change is MAJOR under the Governance section.

**Reason**: on 2026-11-13 the user ruled that only harm CedeGrid adds counts, measured against the
same own work started plainly without CedeGrid in the same place, and that giving another user
more than plain Linux gives is a loss to fix. Measured on the test host on 2026-11-13 against
plain Linux, Performance today costs own work in three shapes:

| Shape | Own work, Performance against plain Linux |
|---|---|
| CPU, one own process against one other on a core | no difference (within a repeat's spread) |
| CPU, 8 own processes wanting 4 CPUs against another user wanting 2 | −3% to −5% CPU in Tracked, −8% to −11% in Full, beside the common plain result |
| GPU, two own probes against one saturating user | 0.475–0.486 of the pair alone, against 0.660 without CedeGrid (−27%) |
| Memory, a host-wide OOM another user causes | about 4 GiB of own memory's 9 given back and two own tasks stopped; the other user's allocator was still killed |

Today's Principle II requires every one of those give-backs ("Own work MUST be the first to give
capacity back", Performance's fair-share limit, the over-share floor rule), so the fix cannot be
made inside a feature (Development Workflow: "A proposed relaxation is raised as an amendment").

**How it fits with feature 005's draft** (`specs/005-system-mode/drafts/amendment-principle-v.md`):
that draft redefines Principle V and leaves Principles I and II as they are. The two touch
different principles and no common sentence. If the user approves both before either lands, they
go in one revision, 8.0.0, with one Sync Impact Report. If one is approved first, it is 8.0.0 and
the other 9.0.0.

**What the same change must also carry** (Governance: relaxing a principle updates
`docs/guarantees.md` and the affected tests in the same change): the spec, guarantee and gate
changes listed in [057-spec-delta.md](057-spec-delta.md).

## Open points this text depends on

The text below is written for the recommended answer of each point in
[057-report.md](057-report.md). Where another answer changes the text, the variant is given in
place. Points 2, 3, 4 and 6 change the spec and the code, not the constitution's wording; point 6
changes the baseline's sentence only if the user picks its option B.

| Point | Recommended | Changes this text |
|---|---|---|
| 1. Memory at a floor breach others caused | B: Performance gives nothing back, the kernel decides | the floors bullet, OOM order, loss by mode, risk bullet, rationale |
| 5. What Aggressive is | B: protected = Performance with 0% margin and fill at once; unprotected = may also start new own work where another principal contends | the modes bullet, the exception bullet, risk bullet |
| 6. The baseline itself | A: the same own work CedeGrid runs | the baseline bullet (B adds a sentence) |

## The diff

### Purpose

```diff
 CedeGrid distributes a shared server's resources efficiently, so that no capacity sits idle. It
 is most useful where other people's processes already run and it is unclear how much to take.
 Its core purpose is that own work runs at the highest possible performance, efficiently, with
 nothing left idle on the server. The modes set how much own work gives back to others:
 Performance, the default, carries this purpose, and Yield First is one option among them.
+What CedeGrid costs anyone, own work or another workload, is measured against the same own work
+run without CedeGrid.
```

### Principle II

```diff
 ### II. No Harm in Any Mode
 
-- Own work MUST be the first to give capacity back when external demand needs it.
-- CedeGrid MUST NOT drive another workload into OOM kills, swap thrash, failed GPU allocations
-  or loss of capacity that workload is already using. Every mode keeps explicit floors, such as
-  minimum available host RAM and per-device VRAM headroom, each with an enforced minimum.
+- **The baseline.** Harm is what CedeGrid adds. Every comparison, for own work and for other
+  workloads, is against the same own work started plainly without CedeGrid in the same place:
+  the same account, control group and session layout. What another workload gets beside that
+  baseline is the kernel's and the GPU driver's split, not CedeGrid's doing.
+- In Yield First and Neutral, own work MUST be the first to give capacity back when external
+  demand needs it.
+- In Performance and Aggressive, CedeGrid MUST NOT cap, pause or drain running own work to give
+  another workload more CPU or GPU compute time than the kernel and the GPU driver would give it
+  beside the same own work.
+- **Admission.** In every mode except unprotected Aggressive, CedeGrid MUST NOT start new own
+  work on a compute resource where it would take another principal below the lesser of that
+  principal's demand and its fair share, nor into activity of unknown amount. In every mode it
+  MUST NOT start new own work into the memory floors.
+- CedeGrid MUST NOT drive another workload into OOM kills, swap thrash, failed GPU allocations
+  or loss of capacity that workload is already using, beyond what the baseline does. Every mode
+  keeps explicit floors, such as minimum available host RAM and per-device VRAM headroom, each
+  with an enforced minimum.
-- **Floors and the memory fair share.** Own memory above its fair share MUST give way at the
-  floors in every mode. In Yield First and Neutral, own memory within its fair share gives way
-  at the floors too. In Performance and Aggressive, own memory within its fair share is not
-  given up for a floor breach that others' growth caused, so the kernel MAY then OOM-kill
-  another workload's process, as it would without CedeGrid. This is the one memory exception to
-  the two rules above.
-- The one compute exception to the two rules above is unprotected Aggressive, below, and it
-  covers CPU and GPU compute time only.
+- **Floors by mode.** In Yield First and Neutral, own memory gives way at the floors: own memory
+  above its fair share first, then the rest. In Performance and Aggressive, own memory is not
+  hinted, drained or sent TERM for a floor breach, and the kernel decides at exhaustion, as it
+  would beside the same own work without CedeGrid; the floors still bound what own work is
+  admitted to, and the operator's envelope applies in every mode.
-- **OOM order by fault.** Own work within its memory fair share MUST stay at the OOM score
-  adjustment it launched with, the same baseline as every other process, and MUST NOT be set
-  below it. The own tasks that make up own memory above its fair share MUST be marked as
-  preferred OOM victims, in the ladder's victim order and for every process in each task's
-  family, until own memory is back within its share, and then return to their baseline. So an
-  OOM that own work causes takes own excess first. Without privileges, in-share own work is
-  protected only up to everyone else's baseline: the kernel can still take it when it is the
-  largest process. An explicit opt-in MAY mark every own process as a preferred victim; that
-  risk falls only on own work.
+- **OOM order.** Own work MUST stay at the OOM score adjustment it launched with, the same
+  baseline as every other process, and MUST NOT be set below it. In Yield
+  First and Neutral, the own tasks that make up own memory above its fair share MUST be marked as
+  preferred OOM victims, in the ladder's victim order and for every process in each task's
+  family, until own memory is back within its share, and then return to their baseline. In
+  Performance and Aggressive no own task is marked, so the kernel chooses as it would without
+  CedeGrid. Without privileges, own work is protected only up to everyone else's baseline: the
+  kernel can still take it when it is the largest process. An explicit opt-in MAY mark every own
+  process as a preferred victim; that risk falls only on own work.
 - Own priority MUST NOT be raised above the default: no negative nice values and no real-time
   scheduling classes. No exception relaxes this rule.
-- **Modes.** The modes are ordered Yield First ≤ Neutral ≤ Performance ≤ Aggressive.
-  Performance is the default mode. Aggressive is a *caution* option with a red label. Its
-  default form, protected Aggressive, keeps every rule of this principle. Any other active
-  principal that is below the lesser of its demand and its fair share gets that CPU and GPU
-  compute back no slower than Performance would give it back, and protected Aggressive never
-  holds less than Performance would in the same observation.
+- **Modes.** The modes are ordered Yield First ≤ Neutral ≤ Performance ≤ Aggressive.
+  Performance is the default mode. Aggressive is a *caution* option with a red label: Performance with no
+  admission margin, filling idle capacity at once. Its default form, protected Aggressive, keeps
+  every rule of this principle, and never holds less than Performance would in the same
+  observation.
-- **Loss by mode.** Yield First and Neutral MAY cost own work compared with running it without
-  CedeGrid. Performance and Aggressive MUST NOT cost own work anything beyond what keeps every
-  other principal at the lesser of its demand and its fair share, plus the documented idle
-  margin (Performance 2%, Aggressive 0%) and CedeGrid's measured own overhead. The
-  work-conserving rule below holds in every mode.
+- **Loss by mode.** Yield First and Neutral MAY cost own work compared with the baseline.
+  Performance and Aggressive MUST NOT cost running own work anything compared with the baseline
+  beyond CedeGrid's measured own overhead. What they hold back is held at admission only: the
+  documented admission margin (Performance 2%, Aggressive 0%) and, except under unprotected
+  Aggressive, new work where another principal contends. The work-conserving rule below holds in
+  every mode.
-- **Unprotected Aggressive, the one compute exception.** It MAY decline to give back CPU and GPU
-  compute time, as an exception to "Own work MUST be the first to give capacity back" and to
-  "loss of capacity that workload is already using". It covers compute time only:
-  - the floors (as above), Principle I, the OOM order and the no-priority-raise rule hold in
-    full;
-  - CedeGrid only declines to give compute back. It MUST NOT raise priority, inflate thread
-    counts or do anything else to win share; the split is whatever the kernel and the GPU
-    driver decide;
+- **Unprotected Aggressive, the one admission exception.** It MAY start new own work on a
+  compute resource another principal contends for, as an exception to the admission rule
+  above. It covers starting compute work only:
+  - the floors (as above), Principle I, the OOM order and the no-priority-raise rule hold in
+    full, and it never starts own work into activity of unknown amount or into the floors;
+  - CedeGrid only starts work. It MUST NOT raise priority, inflate thread counts or do
+    anything else to win share; the split is whatever the kernel and the GPU driver decide;
   - it is never a default. Configuration files, saved per-job setup, profiles, environment
     variables, upgrades, migrations and the node cap MUST NOT enable it. Only an explicit
     per-run flag, or the same explicit per-submission SDK field, turns it on, and only after
     the two-step confirmation the feature specification defines;
   - while active it MUST be reported in `ps`, `top`, logs and JSON output.
 - **Who carries the risk.** An option whose risk falls only on own work, such as a shorter hint
-  deadline or draining Guaranteed work sooner, MAY be offered. Apart from the floors rule for
-  in-share own memory in Performance and Aggressive, above, an option whose risk is OOM kills,
-  swap thrash, failed GPU allocations or any other failure of external workloads MUST NOT exist,
-  not even as an opt-in. The only risk to other workloads that any option may carry is slower
-  compute, and only under unprotected Aggressive.
+  deadline or draining Guaranteed work sooner, MAY be offered. An option whose risk is OOM kills,
+  swap thrash, failed GPU allocations or any other failure of external workloads beyond the
+  baseline MUST NOT exist, not even as an opt-in. The only risk beyond the baseline to other
+  workloads that any option may carry is slower compute from new own work started where they
+  contend, and only under unprotected Aggressive.
 - Options that let own work hold more capacity within the floors and the reclaim bounds, such as
   protected Aggressive or a floor near its minimum, are marked *caution*. Each
   such option MUST:
 ...unchanged...
 - **Work-conserving.** Within these limits, capacity no workload wants MUST NOT sit idle. A
   reserve held back from own work is justified only by how slowly its resource can be
   reclaimed: by default, CPU and GPU compute, reclaimed within a monitor interval, keep only
-  small idle margins, and RAM and GPU memory keep the floors. A more yielding mode the operator
-  chooses MAY keep more idle, since that cost falls only on own work.
+  small idle margins at admission, and RAM and GPU memory keep the floors. A more yielding mode
+  the operator chooses MAY keep more idle, since that cost falls only on own work.
 
-Rationale: running CedeGrid must never make another workload fail through own work beyond its
-fair share. Within its fair share own work is protected as much as others' work: dying first
-because of someone else's growth is a design error, so in Performance and Aggressive in-share
-own memory is kept and the kernel decides, as it would without CedeGrid. An operator may trade
-away their own work's progress in a more yielding mode, but CedeGrid never takes more than the
-fair share at another workload's expense. Capacity held back without a reclaim reason protects
-no workload. Some operators accept
-slowing other workloads' compute to meet a deadline; that choice stays explicit, rare, visible
-and limited to compute time, and it never threatens another workload's safety.
+Rationale: running CedeGrid must never make another workload worse off than the same own work
+run without CedeGrid would. Performance, the default, therefore gives back nothing the kernel
+and the driver would not, and protects own work as much as anyone's: dying first because of
+someone else's growth is a design error. CedeGrid still never adds own work where another
+principal is short of its fair share, never fills the floors, and never touches another
+process. An operator may trade away their own work's progress in a more yielding mode: Neutral
+gives back down to own work's fair share, and Yield First gives others first. Capacity held back
+without a reclaim reason protects no workload. Some operators accept starting work where others
+contend to meet a deadline; that choice stays explicit, rare and visible, and it never
+threatens another workload's safety.
```

The version line:

```diff
-**Version**: 7.0.0 | **Ratified**: 2026-10-24 | **Last Amended**: 2026-10-29
+**Version**: 8.0.0 | **Ratified**: 2026-10-24 | **Last Amended**: <the merge date>
```

## Variants

### Point 1, option A (keep today's floor rule in Performance)

The floors bullet and the OOM bullet keep their 7.0.0 text: own memory above its fair share gives
way at the floors in every mode and is marked a preferred OOM victim; in Performance and
Aggressive in-share own memory is kept at a breach others caused. Then:

- the "Floors by mode" and "OOM order" bullets above are not taken;
- "Loss by mode" adds, after "measured own overhead": "and own memory above its fair share at a
  floor breach others caused";
- "Who carries the risk" keeps 7.0.0's "Apart from the floors rule for in-share own memory".

### Point 1, option C (the kernel decides, except own growth during a breach)

As above, with one sentence added to "Floors by mode": "In Performance and Aggressive, own memory
that grows while a floor is breached gives way, and its tasks are marked as preferred OOM
victims, until own memory is back at what it held when the breach began." "Loss by mode" adds,
after "measured own overhead": "and own growth while a floor is breached".

### Point 5, option A (no unprotected form)

The "Unprotected Aggressive, the one admission exception" bullet is deleted, the admission bullet
reads "In every mode, CedeGrid MUST NOT start…", "Loss by mode" ends its third sentence at "…the
documented admission margin (Performance 2%, Aggressive 0%) and new work where another principal
contends." (the clause "except under unprotected Aggressive" goes), and the last sentence of "Who
carries the risk" reads: "No option carries a risk to other workloads beyond the baseline." The
rationale's last sentence is deleted.

### Point 5, option C (no Aggressive mode)

As point 5 option A, and the modes bullet becomes "Yield First ≤ Neutral ≤ Performance", with the
Aggressive sentences and protected Aggressive in the caution example deleted.

### Point 6, option B (a narrower baseline on hosts that split by thread or context)

The baseline bullet adds: "Where the kernel splits CPU by thread among accounts, or a GPU is
time-sliced among contexts, own work counts in the baseline with at most as many processes on
the resource as keep a principal that arrives later at its fair share by count; CedeGrid admits
no more there." (Report, point 6.)

## Check against the other principles

- **I (untouchable processes)**: unchanged. The draft removes give-backs; it adds no actuator.
- **III (unknown)**: unchanged text. Performance's response to unknown becomes "block expansion,
  then hold" (spec delta, point 2), which is graduated and bounded; Principle III gives the four
  steps as an example and requires escalation to be bounded, not to reach drain.
- **IV (pure policy)**: unchanged; the change is a narrower set of decisions.
- **V (rootless)**: unchanged by this draft; 005 changes it separately.
- **VI (tests first)**: the change needs failing tests first (spec delta, "Tests and gates").
- **VII (compatibility)**: no wire or config key is removed; `performance_margin_percent` keeps
  its meaning for admission. Spec delta, "Configuration".
- **VIII (docs)**: `docs/guarantees.md` changes in the same pull request (spec delta).
