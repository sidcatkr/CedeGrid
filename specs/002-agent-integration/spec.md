# Feature Specification: Agent Integration (Help Docs and MCP Servers)

**Feature Branch**: `001-contention-modes`, specified alongside it. The spec directory is
independent of the branch.

**Created**: 2026-10-24

**Status**: Draft

**Input**: Maintainer feature description. Agents such as OpenAI Codex, Claude Code and other
MCP-capable agents can set up and use CedeGrid through:
- agent-facing help docs
- a machine-readable command interface
- MCP servers

This feature builds on the command-line setup defined in
[001-contention-modes](../001-contention-modes/spec.md) (user story 7, FR-046–FR-055) and does
not redefine setup choices. Implementation candidates are in [plan-notes.md](plan-notes.md).

## Terms

- **Agent**: an AI assistant that runs commands or calls tools for a person. Examples are coding
  agents in a terminal and MCP clients.
- **Operator**: the person who installs and configures CedeGrid and decides what an agent may
  do.
- **MCP server**: a CedeGrid process that offers CedeGrid operations as Model Context Protocol
  tools.
- **Read-only tool**: a tool that cannot change CedeGrid state, configuration, jobs or
  processes.
- **State-changing tool**: a tool that can change any of those, such as writing setup, submitting
  or cancelling jobs, retrying, draining or resuming.
- **Agent guide**: the concise, task-oriented documentation written for agents.
- **Command reference**: the machine-readable description of every public command, its options
  and its structured output, generated from the command definitions.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - An agent learns to use CedeGrid from its guide (Priority: P1)

An agent that has never seen CedeGrid reads the agent guide and the command reference. It then
completes common tasks without human help:
- install
- non-interactive setup
- validate the configuration
- observe, doctor and status
- submit and monitor a job
- drain and resume
- read results

**Why this priority**: agents can only use CedeGrid safely if they can find the correct,
current way to do each task. Without that, they guess flags or parse prose.

**Independent Test**: give an agent only the agent guide and the command reference, in a clean
sandbox with a local test deployment. Ask it to run the quickstart, and check the results and
that no undocumented command was used.

**Acceptance Scenarios**:

1. **Given** an installed CedeGrid, **When** an agent asks the CLI for its agent guide, **Then**
   it gets the guide offline, matching the installed version.
2. **Given** the command reference, **When** compared with the installed command definitions,
   **Then** every public command, option and output field matches exactly.
3. **Given** the guide's setup recipe, **When** an agent follows it on a non-TTY, **Then** setup
   completes from flags alone and never waits for input.
4. **Given** the guide, **When** an agent looks for what it must not do, **Then** the guide lists
   the safety boundaries:
   - CedeGrid never acts on other processes;
   - *danger* setup options need an explicit acknowledgement;
   - state-changing operations may be disabled by the operator.

---

### User Story 2 - An agent inspects CedeGrid through MCP (Priority: P1)

An operator registers the CedeGrid MCP server with their agent. Out of the box, the agent can
look but not change anything:
- observe telemetry
- run doctor
- read node and job status
- validate a configuration
- explain the current yield decisions
- list jobs and read results

**Why this priority**: inspection is the most common agent use and carries no risk. It has to
work safely by default.

**Independent Test**: start the MCP server with default settings and list its tools. Every
listed tool must be read-only, and calling each one must leave state, configuration and
processes unchanged.

**Acceptance Scenarios**:

1. **Given** default settings, **When** an agent lists tools, **Then** only read-only tools are
   offered, and each carries a read-only annotation.
2. **Given** a read-only tool call, **When** it completes, **Then** the result is structured data
   with the same content as the equivalent command's structured output.
3. **Given** a tool result that contains workload output (logs, results or artifact metadata),
   **When** it is returned, **Then** that content is marked as untrusted data, not instructions.
4. **Given** a coordinator query, **When** the server forwards it, **Then** it uses the
   operator's existing client configuration and certificate role. It never adds authority, and
   never returns key material or certificate contents.

---

### User Story 3 - The operator lets an agent make specific changes (Priority: P1)

An operator explicitly enables selected state-changing tools, for example setup and job
submission but not drain. The agent can then perform those changes within the same rules as the
command line, and every change is recorded.

**Why this priority**: agents become useful for running work only when they can change state,
but that power must be granted deliberately, per operation, by a person.

**Independent Test**: enable one state-changing tool and call it. Check that:
- it succeeds and is recorded;
- disabled tools are neither offered nor callable;
- a *danger* setup choice is refused without its acknowledgement.

**Acceptance Scenarios**:

1. **Given** the operator enables a state-changing tool, **When** the agent lists tools, **Then**
   that tool appears with annotations that say whether it is destructive and whether it is
   idempotent.
2. **Given** a tool that is not enabled, **When** an agent calls it by name, **Then** the call is
   refused with a stable error code, and nothing changes.
3. **Given** the setup tool is enabled, **When** an agent requests a *danger* option without the
   explicit acknowledgement argument, **Then** the call is refused and nothing is written. With
   the acknowledgement, the result reports the active exception and its marker.
4. **Given** any state-changing call, **When** it completes or fails, **Then** an audit record
   holds:
   - the time and the tool
   - the arguments, with secrets redacted
   - the outcome
5. **Given** an MCP client, **When** it tries to enable more tools itself, **Then** it cannot.
   Only the operator's configuration or server start options control which tools are enabled.

---

### User Story 4 - Start the MCP server with one command (Priority: P2)

An operator who installed CedeGrid through any supported package starts the MCP server with one
documented command, and pastes a documented snippet into their agent's configuration.

**Why this priority**: setup friction decides whether the integration is used at all.

**Independent Test**: for each package (native binary, npm, Python), run the documented start
command and complete an MCP handshake. Check that each documented client snippet is valid for
its client's configuration format.

**Acceptance Scenarios**:

1. **Given** the native binary, the npm package or the Python package, **When** the operator runs
   the documented start command, **Then** the same MCP server starts and reports the CedeGrid
   version.
2. **Given** the docs, **When** the operator looks for agent configuration, **Then** there are
   ready snippets for Claude Code, Codex and a generic MCP client. Each says which tools are
   enabled by default.
3. **Given** the Python package without a native binary on the machine, **When** the start
   command runs, **Then** it exits with a clear message saying how to install the binary.

---

### User Story 5 - Commands agents can parse (Priority: P2)

Every command an agent uses can return structured output and fails with stable error codes, so
agents act on results without parsing prose.

**Why this priority**: both the agent guide and the MCP tools depend on stable, structured
results.

**Independent Test**: run each agent-facing command in structured mode, successful and failing.
Validate the output against its published schema and check that failures carry a stable code
and exit status.

**Acceptance Scenarios**:

1. **Given** structured output mode, **When** a command succeeds, **Then** its output matches the
   published, versioned schema for that command.
2. **Given** a failure, **When** a command exits, **Then** it reports a stable error code and a
   documented exit status, along with a one-sentence message.
3. **Given** a non-TTY or structured output mode, **When** a command would otherwise prompt,
   **Then** it does not prompt. It fails with a code that names the missing input.

---

### User Story 6 - Coding agents get consistent repository guidance (Priority: P3)

A coding agent working on the CedeGrid repository finds its entry point (`AGENTS.md` for Codex
and other agents that read it, `CLAUDE.md` for Claude Code), and both lead to the same guidance.

**Why this priority**: contributor agents already use `CLAUDE.md`. Codex support is a small,
separate addition.

**Independent Test**: check that `AGENTS.md` and `CLAUDE.md` together contain each piece of
guidance exactly once, and that both point to the constitution.

**Acceptance Scenarios**:

1. **Given** the repository, **When** a coding agent opens `AGENTS.md` or `CLAUDE.md`, **Then** it
   reaches the same commands, invariants and workflow. One file holds the content and the other
   refers to it without duplicating it.

---

### Edge Cases

- **An agent calls a tool while another state-changing call is running.** Calls are applied one
  at a time where the CLI would conflict, and the second call either waits within a bounded
  time or fails with a stable "busy" code.
- **A tool call runs longer than the client's timeout.** The server bounds every call. A
  long-running operation, such as waiting for a job, returns a handle or a status to poll
  instead of blocking.
- **The configuration file is missing.** Read-only tools report that setup is needed. They
  never start setup themselves.
- **The coordinator is unreachable.** The tool fails with a stable connectivity code. Local
  read-only tools keep working.
- **Workload output contains text that looks like instructions.** It is returned only as data,
  marked untrusted.
- **The MCP client disconnects mid-call.** Calls that have not started are cancelled. A call
  already committed is still recorded.
- **Windows.** Node commands are unsupported there, so only client-side tools are offered.
- **The agent guide is requested for a different installed version.** The CLI serves the guide
  for its own version only.

## Requirements *(mandatory)*

### Functional Requirements

**Agent guide and reference**

- **FR-001**: CedeGrid MUST provide an agent guide with task recipes:
  - install
  - non-interactive setup
  - configuration validation
  - observe, doctor and status
  - submitting and monitoring jobs
  - drain and resume
  - reading results
  - safety boundaries
- **FR-002**: The agent guide MUST be available offline from the installed CLI, and MUST be
  included in the native, npm and Python packages for the same version.
- **FR-003**: A machine-readable command reference MUST be generated from the command
  definitions. It covers every public command, option and structured output schema. A CI check
  MUST fail when the reference and the command definitions differ.
- **FR-004**: The agent guide MUST follow constitution Principle VIII: concise, factual, no work
  logs, no private deployment details and no project-motivation narrative.
- **FR-005**: `AGENTS.md` and `CLAUDE.md` MUST give coding agents the same repository guidance
  from a single source, with no duplicated content.

**MCP server**

- **FR-006**: CedeGrid MUST provide an MCP server that offers CedeGrid operations as tools.
- **FR-007**: With default settings, the server MUST offer only read-only tools: observe, doctor,
  node and job status, configuration validation, explaining the current yield decisions,
  listing jobs, and reading results.
- **FR-008**: Each state-changing tool MUST be off by default. It MUST be enabled individually,
  only by the operator's configuration or server start options. An MCP client MUST NOT be able
  to enable tools.
- **FR-009**: Every tool MUST carry annotations stating whether it is read-only, destructive and
  idempotent. Disabled tools MUST NOT be listed. Calls to them MUST be refused with a stable
  error code.
- **FR-010**: Each tool MUST correspond to one documented command-line operation, with the same
  validation, authority and structured result.
  - The server MUST NOT offer anything the CLI cannot do.
  - It MUST NOT expose raw protocol requests, internal hidden commands or arbitrary command
    execution beyond job submission as the CLI defines it.
- **FR-011**: Coordinator operations MUST use the operator's existing client configuration and
  certificate role. The server MUST NOT create credentials, widen roles or return key material
  or certificate contents.
- **FR-012**: The setup tool MUST use the non-interactive setup from 001.
  - A *danger* option MUST require the same explicit acknowledgement argument as the CLI flag.
  - Results MUST report active exceptions with their markers.
- **FR-013**: Every state-changing call MUST be recorded in an audit log under CedeGrid's state
  directory. Each record holds the time, the tool, the arguments with secrets redacted, the
  outcome and the resulting state change.
- **FR-014**: Tool results MUST mark workload-originated content (logs, results, artifact
  metadata) as untrusted data.
- **FR-015**: Every tool call MUST be bounded in time and output size. A long operation MUST
  return a status or handle to poll instead of blocking. No tool MAY prompt for interactive
  input.
- **FR-016**: Errors MUST be returned as structured tool errors with the same stable codes as
  the CLI.
- **FR-017**: The server MUST accept connections only from its local client, in this feature.
  It MUST NOT open a network listener.
- **FR-018**: All CedeGrid safety invariants MUST hold through the MCP server:
  - no action on external processes
  - launch barrier
  - verified ownership
  - unknown telemetry never treated as free

  Tests MUST show that no tool can reach an operation outside FR-010.

**Distribution**

- **FR-019**: The native binary MUST start the MCP server with one documented command. The npm
  package and the Python package MUST offer the same one-command start by launching the
  installed native binary of the same version.
- **FR-020**: When the native binary is missing, the npm and Python start commands MUST exit with
  a message that says how to install it.
- **FR-021**: The docs MUST provide client configuration snippets for Claude Code, Codex and a
  generic MCP client. They MUST state which tools are enabled by default and how an operator
  enables more.
- **FR-022**: The server MUST report the CedeGrid version and its enabled tools during the MCP
  handshake.

**Machine-readable command interface**

- **FR-023**: Every command used by the agent guide or an MCP tool MUST support structured
  output with a published, versioned schema. Schema changes MUST be backward compatible within a
  major version.
- **FR-024**: Every such command MUST fail with a stable error code and a documented exit status,
  alongside a one-sentence message.
- **FR-025**: In structured output mode or on a non-TTY, no command MAY prompt. A missing input
  MUST produce an error code that names it.

### Key Entities

- **Agent guide**: versioned, task-oriented documentation for agents. It is shipped with every
  package and served by the CLI.
- **Command reference**: generated description of commands, options, outputs, error codes and
  exit statuses, checked against the command definitions.
- **Tool**: an MCP operation with a name, a description, a mapping to one CLI operation,
  annotations (read-only, destructive, idempotent) and an enabled flag.
- **Tool policy**: the operator's choice of enabled state-changing tools, held in configuration
  or given as server start options.
- **Audit record**: one state-changing call, with its time, tool, redacted arguments, outcome and
  resulting change.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A reference agent given only the agent guide and the command reference completes
  the scripted quickstart without human help in at least 9 of 10 runs. The quickstart is: setup
  with flags, observe, submit a sample job, read its status, read its result.
- **SC-002**: 100% of the read-only operations used in the agent guide are available as MCP
  tools.
- **SC-003**: With default settings, zero offered tools can change state, as verified by a test
  that lists and calls every tool.
- **SC-004**: 100% of state-changing tool calls in the test suite produce an audit record.
  Zero audit records contain secrets.
- **SC-005**: The command reference and the command definitions differ in zero entries in CI.
- **SC-006**: Each supported package starts the MCP server with one documented command, and the
  handshake succeeds on Linux and macOS.
- **SC-007**: 100% of agent-facing commands validate against their published schemas, in both
  success and failure cases.
- **SC-008**: Zero actions on external processes, and zero operations beyond FR-010, are
  reachable through the MCP server in the test and pressure suites.

## Assumptions

- The MCP server serves one local client over standard input and output. Remote or networked
  transports are out of scope for this feature.
- The operator is the person who installs and configures CedeGrid. Agents act with the
  operator's local user rights and existing client certificates, never more.
- The Python package stays a pure SDK. Its MCP start command finds and launches the installed
  native binary rather than reimplementing CedeGrid.
- The agent guide is written in English and versioned with the release.
- Interactive setup from 001 remains the default for people. Agents use the flag interface.
- Existing error codes (`ERR_CEDEGRID_*`) are extended rather than replaced.
