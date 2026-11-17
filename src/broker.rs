//! System mode's root broker (feature 005).
//!
//! The broker starts and stops a `cedegrid` group member's two units, moves a member's job
//! between them, and writes OOM scores inside them. Nothing else runs as root. The modules here
//! are the request parser, the administrator policy, the unit builder, the score function, the
//! process handle the writes go through, the peer check, the systemd calls and the listener.
//! Moves and the OOM loop land with their tasks.

pub mod admin_policy;
#[cfg(unix)]
pub mod peer;
#[cfg(target_os = "linux")]
pub mod process_handle;
pub mod request;
pub mod score;
#[cfg(unix)]
pub mod server;
pub mod systemd;
pub mod units;

use serde::{Deserialize, Serialize};

/// The broker protocol version. Client and broker refuse any other (FR-035).
pub const PROTOCOL: u32 = 1;

/// The only socket the broker serves, made by `cedegrid-broker.socket` (FR-002).
pub const SOCKET_PATH: &str = "/run/cedegrid/broker.sock";

/// The group whose members the broker serves (FR-004).
pub const GROUP: &str = "cedegrid";

/// The root-owned installed binary every unit runs. No unit runs a path from a request (FR-006).
pub const INSTALLED_BINARY: &str = "/usr/bin/cedegrid";

/// Hidden subcommand the broker's own unit starts; dispatched before argument parsing.
pub const BROKER_SUBCOMMAND: &str = "__system-broker";

/// Hidden subcommand the ordinary unit runs: the user's agent on the delegated Full path.
pub const AGENT_SUBCOMMAND: &str = "__system-agent";

/// Hidden subcommand the idle unit runs: the executor the user's agent drives (research R9).
pub const IDLE_EXECUTOR_SUBCOMMAND: &str = "__idle-executor";

/// A contention mode as the broker and the administrator policy name it.
///
/// Aggressive is listed so the policy can name it; it is allowed only where the administrator
/// lists it (contracts/admin-policy.md).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModeName {
    YieldFirst,
    Neutral,
    Performance,
    Aggressive,
}

impl ModeName {
    pub const ALL: [ModeName; 4] = [
        ModeName::YieldFirst,
        ModeName::Neutral,
        ModeName::Performance,
        ModeName::Aggressive,
    ];

    /// The name in requests, the policy and logs.
    pub fn name(self) -> &'static str {
        match self {
            ModeName::YieldFirst => "yield_first",
            ModeName::Neutral => "neutral",
            ModeName::Performance => "performance",
            ModeName::Aggressive => "aggressive",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.name() == name)
    }
}
