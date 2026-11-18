//! The broker's closed request set and its framing (contracts/broker-protocol.md).
//!
//! A request names no identity: the caller is the socket's peer, never a field (FR-003). Anything
//! outside the closed set refuses the whole request.

use super::{ModeName, PROTOCOL};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The most the broker reads from one connection, the newline included (FR-008).
pub const MAX_REQUEST_BYTES: usize = 65_536;

/// The longest job identifier a move may name.
pub const MAX_JOB_BYTES: usize = 128;

/// Field names, and parts of field names, that would name an identity. A request carrying one is
/// refused as a whole and logged at warning level (FR-003).
const IDENTITY_WORDS: &[&str] = &[
    "uid",
    "gid",
    "euid",
    "user",
    "username",
    "group",
    "groups",
    "pid",
    "pidfd",
    "unit",
    "slice",
    "service",
    "path",
    "cgroup",
    "exec",
    "executable",
    "command",
    "argv",
    "mount",
    "device",
    "capability",
    "capabilities",
    "caps",
];

const FIELDS: &[&str] = &["protocol", "kind", "mode", "job", "on"];

/// One request about the caller's own units (FR-005).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// Start the caller's two units; a mode, when given, is checked against the policy.
    Start { mode: Option<ModeName> },
    /// Stop the caller's two units.
    Stop,
    /// Report the caller's units.
    Status,
    /// Move one of the caller's jobs to the unit for `mode` (FR-018).
    Move { job: JobId, mode: ModeName },
    /// Raise, or stop raising, the CPU weight of the caller's account slice under unprotected
    /// (006 FR-016).
    Raise { on: bool },
}

impl Request {
    /// The request as one JSON line, as the client sends it.
    pub fn to_line(&self) -> String {
        let mut fields = Map::new();
        fields.insert("protocol".to_owned(), Value::from(PROTOCOL));
        fields.insert("kind".to_owned(), Value::from(self.kind()));
        if let Some(mode) = self.mode() {
            fields.insert("mode".to_owned(), Value::from(mode.name()));
        }
        if let Request::Move { job, .. } = self {
            fields.insert("job".to_owned(), Value::from(job.as_str()));
        }
        if let Request::Raise { on } = self {
            fields.insert("on".to_owned(), Value::from(*on));
        }
        let mut line = Value::Object(fields).to_string();
        line.push('\n');
        line
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Request::Start { .. } => "start",
            Request::Stop => "stop",
            Request::Status => "status",
            Request::Move { .. } => "move",
            Request::Raise { .. } => "raise",
        }
    }

    pub fn mode(&self) -> Option<ModeName> {
        match self {
            Request::Start { mode } => *mode,
            Request::Move { mode, .. } => Some(*mode),
            Request::Stop | Request::Status | Request::Raise { .. } => None,
        }
    }
}

/// A job identifier the agent gave a run: 1 to 128 bytes of `[A-Za-z0-9_-]`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct JobId(String);

impl JobId {
    pub fn parse(value: &str) -> Option<Self> {
        let valid = !value.is_empty()
            && value.len() <= MAX_JOB_BYTES
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        valid.then(|| JobId(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Every reason the broker refuses a request (contracts/broker-protocol.md, "Refusal codes").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalCode {
    ProtocolMismatch,
    NotAMember,
    RefusedAccount,
    RequestTooLarge,
    RequestTimeout,
    Malformed,
    UnknownField,
    IdentityInRequest,
    UnknownKind,
    ModeNotAllowed,
    TaskLimit,
    /// The administrator policy file is refused, so the broker serves no request.
    PolicyRefused,
    RateLimited,
    UnitNotOurs,
    JobNotFound,
    MoveRefused,
    /// The unprotected raise is not permitted, or reaches no other account from here (006).
    RaiseRefused,
    SystemdTimeout,
    SystemdFailed,
}

/// A refusal: its code and one sentence that says what to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub code: RefusalCode,
    pub message: String,
}

impl Refusal {
    pub fn new(code: RefusalCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// One of the caller's units in a reply.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnitStatus {
    pub kind: super::units::UnitKind,
    pub name: String,
    pub state: String,
    pub cgroup: String,
}

/// The broker's one reply per connection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub protocol: u32,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub units: Option<Vec<UnitStatus>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<RefusalCode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl Reply {
    pub fn units(units: Vec<UnitStatus>) -> Self {
        Self {
            protocol: PROTOCOL,
            ok: true,
            units: Some(units),
            code: None,
            message: None,
        }
    }

    pub fn refused(refusal: &Refusal) -> Self {
        Self {
            protocol: PROTOCOL,
            ok: false,
            units: None,
            code: Some(refusal.code),
            message: Some(refusal.message.clone()),
        }
    }

    /// The reply as one JSON line, as the broker writes it.
    pub fn to_line(&self) -> String {
        // A struct of strings, integers and enums always serializes.
        let mut line = serde_json::to_string(self).unwrap_or_else(|_| {
            format!(r#"{{"protocol":{PROTOCOL},"ok":false,"code":"malformed"}}"#)
        });
        line.push('\n');
        line
    }
}

/// Parses everything the broker read from one connection.
///
/// `frame` holds at most `MAX_REQUEST_BYTES + 1` bytes: the reader stops after the first newline
/// or once the limit is passed. The request is the bytes before the newline, the newline counting
/// toward the limit.
pub fn parse_frame(frame: &[u8]) -> Result<Request, Refusal> {
    let body = match frame.iter().position(|&byte| byte == b'\n') {
        Some(end) if end >= MAX_REQUEST_BYTES => return Err(too_large()),
        Some(end) if end + 1 != frame.len() => {
            return Err(Refusal::new(
                RefusalCode::Malformed,
                "more than one request on the connection; send one request per connection",
            ));
        }
        Some(end) => &frame[..end],
        None if frame.len() >= MAX_REQUEST_BYTES => return Err(too_large()),
        None => {
            return Err(Refusal::new(
                RefusalCode::Malformed,
                "request not terminated by a newline",
            ));
        }
    };
    let value: Value = serde_json::from_slice(body).map_err(|_| {
        Refusal::new(
            RefusalCode::Malformed,
            "request is not a single-line JSON object",
        )
    })?;
    let Value::Object(fields) = value else {
        return Err(Refusal::new(
            RefusalCode::Malformed,
            "request is not a single-line JSON object",
        ));
    };
    if let Some(name) = fields.keys().find(|name| names_identity(name)) {
        return Err(Refusal::new(
            RefusalCode::IdentityInRequest,
            format!(
                "field {} not accepted: callers are identified by socket credentials only",
                shown(name)
            ),
        ));
    }
    check_protocol(&fields)?;
    if let Some(name) = fields.keys().find(|name| !FIELDS.contains(&name.as_str())) {
        return Err(unknown_field(name));
    }
    let kind = match fields.get("kind") {
        Some(Value::String(kind)) => kind.as_str(),
        _ => {
            return Err(Refusal::new(
                RefusalCode::Malformed,
                "missing or invalid field kind (expected start, stop, status, move or raise)",
            ));
        }
    };
    match kind {
        "start" => {
            only_fields(&fields, &["protocol", "kind", "mode"])?;
            let mode = fields.get("mode").map(mode_value).transpose()?;
            Ok(Request::Start { mode })
        }
        "stop" => {
            only_fields(&fields, &["protocol", "kind"])?;
            Ok(Request::Stop)
        }
        "status" => {
            only_fields(&fields, &["protocol", "kind"])?;
            Ok(Request::Status)
        }
        "move" => {
            let job = match fields.get("job") {
                Some(Value::String(job)) => JobId::parse(job).ok_or_else(|| {
                    Refusal::new(
                        RefusalCode::Malformed,
                        "invalid job ID (expected 1 to 128 characters from [A-Za-z0-9_-])",
                    )
                })?,
                _ => {
                    return Err(Refusal::new(
                        RefusalCode::Malformed,
                        "move request without field job",
                    ));
                }
            };
            let mode = match fields.get("mode") {
                Some(value) => mode_value(value)?,
                None => {
                    return Err(Refusal::new(
                        RefusalCode::Malformed,
                        "move request without field mode",
                    ));
                }
            };
            Ok(Request::Move { job, mode })
        }
        "raise" => {
            only_fields(&fields, &["protocol", "kind", "on"])?;
            match fields.get("on") {
                Some(Value::Bool(on)) => Ok(Request::Raise { on: *on }),
                _ => Err(Refusal::new(
                    RefusalCode::Malformed,
                    "raise request without a true or false field on",
                )),
            }
        }
        _ => Err(Refusal::new(
            RefusalCode::UnknownKind,
            "unknown request kind (expected start, stop, status, move or raise)",
        )),
    }
}

/// Whether a field name, or any part of it, names an identity (FR-003).
pub fn names_identity(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .chain(std::iter::once(lower.as_str()))
        .any(|word| IDENTITY_WORDS.contains(&word))
}

fn check_protocol(fields: &Map<String, Value>) -> Result<(), Refusal> {
    let Some(protocol) = fields.get("protocol").and_then(Value::as_u64) else {
        return Err(Refusal::new(
            RefusalCode::Malformed,
            format!("missing or invalid field protocol (expected the integer {PROTOCOL})"),
        ));
    };
    match protocol.cmp(&u64::from(PROTOCOL)) {
        std::cmp::Ordering::Equal => Ok(()),
        std::cmp::Ordering::Less => Err(Refusal::new(
            RefusalCode::ProtocolMismatch,
            format!(
                "protocol mismatch: broker {PROTOCOL}, client {protocol}; upgrade the cedegrid client"
            ),
        )),
        std::cmp::Ordering::Greater => Err(Refusal::new(
            RefusalCode::ProtocolMismatch,
            format!(
                "protocol mismatch: broker {PROTOCOL}, client {protocol}; ask an administrator to upgrade cedegrid-system"
            ),
        )),
    }
}

fn only_fields(fields: &Map<String, Value>, allowed: &[&str]) -> Result<(), Refusal> {
    match fields.keys().find(|name| !allowed.contains(&name.as_str())) {
        Some(name) => Err(unknown_field(name)),
        None => Ok(()),
    }
}

fn mode_value(value: &Value) -> Result<ModeName, Refusal> {
    value.as_str().and_then(ModeName::from_name).ok_or_else(|| {
        Refusal::new(
            RefusalCode::Malformed,
            "invalid mode (expected yield_first, neutral, performance or aggressive)",
        )
    })
}

fn unknown_field(name: &str) -> Refusal {
    Refusal::new(
        RefusalCode::UnknownField,
        format!(
            "unknown field {} (this request accepts {})",
            shown(name),
            "protocol, kind, and mode, job or on where the kind takes them"
        ),
    )
}

fn too_large() -> Refusal {
    Refusal::new(
        RefusalCode::RequestTooLarge,
        format!("request exceeds {MAX_REQUEST_BYTES} bytes"),
    )
}

/// A field name as it may appear in a reply or a journal line: quoted when it is short and plain.
fn shown(name: &str) -> String {
    let plain = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if plain {
        format!("'{name}'")
    } else {
        "(name not shown)".to_owned()
    }
}
