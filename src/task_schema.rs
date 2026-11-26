//! The one list of task fields (003 FR-060, contracts/task-schema.md): each field's JSON name,
//! its `run` flag, its Python parameter and TypeScript option, its type and default. `run`'s
//! flags come from it, and a contract fixture holds both SDKs to it.

use serde::Serialize;

/// One task field across every surface that sets it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Field {
    /// The Python parameter of `command_task`, and the name in the schema.
    pub name: &'static str,
    /// The `run` flag; `None` where the command line gives the value (`argv`, `cwd`).
    pub flag: Option<&'static str>,
    /// The TypeScript name: a `commandTask` argument or a `CommandOptions` key.
    pub typescript: &'static str,
    /// The value's type and range, in words.
    pub kind: &'static str,
    /// The default, in words.
    pub default: &'static str,
    /// One sentence for `run --help`.
    pub help: &'static str,
}

/// Every task field, in `command_task`'s order.
pub const FIELDS: &[Field] = &[
    Field {
        name: "task_id",
        flag: Some("--task-id"),
        typescript: "taskId",
        kind: "text, not empty",
        default: "generated",
        help: "The task's name within its job.",
    },
    Field {
        name: "argv",
        flag: None,
        typescript: "argv",
        kind: "list of text, not empty",
        default: "required",
        help: "The command and its arguments.",
    },
    Field {
        name: "cwd",
        flag: None,
        typescript: "cwd",
        kind: "absolute path",
        default: "the current directory",
        help: "The directory the command starts in.",
    },
    Field {
        name: "cpu_millicores",
        flag: Some("--cpu-millicores"),
        typescript: "cpuMillicores",
        kind: "integer, at least 1",
        default: "1000",
        help: "CPU to reserve, in thousandths of a core.",
    },
    Field {
        name: "ram_mib",
        flag: Some("--ram-mib"),
        typescript: "ramMib",
        kind: "integer, at least 1",
        default: "512",
        help: "RAM to reserve, in MiB.",
    },
    Field {
        name: "gpu_vram_mib",
        flag: Some("--gpu-vram-mib"),
        typescript: "gpuVramMib",
        kind: "map of GPU to integer MiB",
        default: "none",
        help: "GPU memory to reserve on each named GPU, in MiB.",
    },
    Field {
        name: "env",
        flag: Some("--env"),
        typescript: "env",
        kind: "map of text to text",
        default: "none",
        help: "Environment values passed at launch and never stored.",
    },
    Field {
        name: "replay_safe",
        flag: Some("--replay-safe"),
        typescript: "replaySafe",
        kind: "true or false",
        default: "false",
        help: "Whether the task may run again from the start after a loss.",
    },
    Field {
        name: "allocation_class",
        flag: Some("--allocation-class"),
        typescript: "allocationClass",
        kind: "guaranteed or opportunistic",
        default: "guaranteed",
        help: "Whether the task keeps its reservation or may be ended to give it back.",
    },
    Field {
        name: "required_controls",
        flag: Some("--required-control"),
        typescript: "requiredControls",
        kind: "list of control names",
        default: "none",
        help: "Controls the task needs; it never starts without them.",
    },
    Field {
        name: "single_process",
        flag: Some("--single-process"),
        typescript: "singleProcess",
        kind: "true or false",
        default: "false",
        help: "Whether the task is one process with no children.",
    },
    Field {
        name: "no_escape",
        flag: Some("--no-escape"),
        typescript: "noEscape",
        kind: "true or false",
        default: "false",
        help: "Whether the task promises that no process leaves its tree.",
    },
    Field {
        name: "managed_child_limit",
        flag: Some("--managed-child-limit"),
        typescript: "managedChildLimit",
        kind: "integer from 0 to 8",
        default: "0",
        help: "How many children the supervisor may start for the task.",
    },
    Field {
        name: "input_artifacts",
        flag: Some("--input-artifact"),
        typescript: "inputArtifacts",
        kind: "list of published artifacts",
        default: "none",
        help: "Published files fetched and checked before the task starts.",
    },
    Field {
        name: "max_attempts",
        flag: Some("--max-attempts"),
        typescript: "maxAttempts",
        kind: "integer, at least 1",
        default: "unbounded",
        help: "The most attempts the task may take, yields included.",
    },
    Field {
        name: "contention_modes",
        flag: Some("--mode"),
        typescript: "contentionModes",
        kind: "map of resource to mode",
        default: "the node's modes",
        help: "The mode per resource; the node cap still applies.",
    },
    Field {
        name: "admit_immediately",
        flag: Some("--admit-immediately"),
        typescript: "admitImmediately",
        kind: "true or false",
        default: "false",
        help: "Start at once beside a CedeGrid user that arrived first (006); needs the confirmation.",
    },
    Field {
        name: "confirm_admit_immediately",
        flag: Some("--confirm-admit-immediately"),
        typescript: "confirmAdmitImmediately",
        kind: "true or false",
        default: "false",
        help: "The second confirmation for --admit-immediately.",
    },
    Field {
        name: "unprotected",
        flag: Some("--unprotected"),
        typescript: "unprotected",
        kind: "true or false",
        default: "false",
        help: "Run unprotected (006); needs the confirmation and the host's allowance.",
    },
    Field {
        name: "confirm_unprotected",
        flag: Some("--confirm-unprotected"),
        typescript: "confirmUnprotected",
        kind: "true or false",
        default: "false",
        help: "The second confirmation for --unprotected.",
    },
];

/// The contract fixture's text: every field, as `tests/contracts/task-schema.json` holds it.
pub fn export() -> String {
    let fields = serde_json::to_value(FIELDS).unwrap_or_default();
    let document = serde_json::json!({ "schema": "cedegrid-task/1", "fields": fields });
    let mut text = serde_json::to_string_pretty(&document).unwrap_or_default();
    text.push('\n');
    text
}
