//! Feature 005, System mode: the broker's pure parts.
//!
//! Request parsing (T006), the administrator policy (T007), the unit builder (T008) and the score
//! function (T042). The container fixtures that need root and systemd come later and are opt-in.

use cedegrid::broker::admin_policy::{self, AdminPolicy, Source};
use cedegrid::broker::request::{
    JobId, MAX_REQUEST_BYTES, RaiseLevel, RefusalCode, Reply, Request, parse_frame,
};
use cedegrid::broker::score::{
    Lowering, OverShareMarking, ProcessKey, ScoreInput, ScoreWrite, ScoredProcess, ScoredTask,
    UNKNOWN_WINDOW, UserMemory, score_writes,
};
use cedegrid::broker::units::{
    self, Caller, OrdinarySlice, PamSession, Placement, UnitKind, systemd_run_args,
};
use cedegrid::broker::{INSTALLED_BINARY, ModeName, PROTOCOL};
use cedegrid::config::SystemAccounts;
use std::time::Duration;

fn frame(json: &str) -> Vec<u8> {
    let mut bytes = json.as_bytes().to_vec();
    bytes.push(b'\n');
    bytes
}

fn refused(json: &str) -> RefusalCode {
    parse_frame(&frame(json)).expect_err(json).code
}

// T006: the request parser.

#[test]
fn closed_request_set_parses() {
    assert_eq!(
        parse_frame(&frame(r#"{"protocol":1,"kind":"start","mode":"neutral"}"#)),
        Ok(Request::Start {
            mode: Some(ModeName::Neutral)
        })
    );
    assert_eq!(
        parse_frame(&frame(r#"{"protocol":1,"kind":"start"}"#)),
        Ok(Request::Start { mode: None })
    );
    assert_eq!(
        parse_frame(&frame(r#"{"protocol":1,"kind":"stop"}"#)),
        Ok(Request::Stop)
    );
    assert_eq!(
        parse_frame(&frame(r#"{"protocol":1,"kind":"status"}"#)),
        Ok(Request::Status)
    );
    assert_eq!(
        parse_frame(&frame(
            r#"{"protocol":1,"kind":"move","job":"job-7_a","mode":"yield_first"}"#
        )),
        Ok(Request::Move {
            job: JobId::parse("job-7_a").unwrap(),
            mode: ModeName::YieldFirst
        })
    );
    // 006 FR-016: the unprotected raise takes only its switch.
    for on in [true, false] {
        assert_eq!(
            parse_frame(&frame(&format!(
                r#"{{"protocol":1,"kind":"raise","on":{on}}}"#
            ))),
            Ok(Request::Raise {
                on,
                level: RaiseLevel::Unprotected
            })
        );
        assert_eq!(
            Request::Raise {
                on,
                level: RaiseLevel::Unprotected
            }
            .to_line(),
            format!("{{\"kind\":\"raise\",\"on\":{on},\"protocol\":1}}\n")
        );
    }
    assert_eq!(
        refused(r#"{"protocol":1,"kind":"raise"}"#),
        RefusalCode::Malformed
    );
    assert_eq!(
        refused(r#"{"protocol":1,"kind":"raise","on":"yes"}"#),
        RefusalCode::Malformed
    );
    assert_eq!(
        refused(r#"{"protocol":1,"kind":"raise","on":true,"mode":"neutral"}"#),
        RefusalCode::UnknownField
    );
    assert_eq!(
        refused(r#"{"protocol":1,"kind":"raise","on":true,"weight":10000}"#),
        RefusalCode::UnknownField
    );
    // 006 FR-027: priority over plain users is the same request at its own level; a request
    // without a level is unprotected's, as before.
    assert_eq!(
        parse_frame(&frame(
            r#"{"protocol":1,"kind":"raise","on":true,"level":"plain"}"#
        )),
        Ok(Request::Raise {
            on: true,
            level: RaiseLevel::Plain
        })
    );
    assert_eq!(
        parse_frame(&frame(
            r#"{"protocol":1,"kind":"raise","on":true,"level":"unprotected"}"#
        )),
        Ok(Request::Raise {
            on: true,
            level: RaiseLevel::Unprotected
        })
    );
    assert_eq!(
        Request::Raise {
            on: true,
            level: RaiseLevel::Plain
        }
        .to_line(),
        "{\"kind\":\"raise\",\"level\":\"plain\",\"on\":true,\"protocol\":1}\n"
    );
    assert_eq!(
        refused(r#"{"protocol":1,"kind":"raise","on":true,"level":"realtime"}"#),
        RefusalCode::Malformed
    );
}

#[test]
fn request_size_limit_counts_the_newline() {
    let request = r#"{"protocol":1,"kind":"status"}"#;
    // Exactly the limit, newline included: accepted.
    let mut at_limit = request.as_bytes().to_vec();
    at_limit.resize(MAX_REQUEST_BYTES - 1, b' ');
    at_limit.push(b'\n');
    assert_eq!(at_limit.len(), MAX_REQUEST_BYTES);
    assert_eq!(parse_frame(&at_limit), Ok(Request::Status));
    // One byte over: refused.
    let mut over = request.as_bytes().to_vec();
    over.resize(MAX_REQUEST_BYTES, b' ');
    over.push(b'\n');
    assert_eq!(over.len(), MAX_REQUEST_BYTES + 1);
    assert_eq!(
        parse_frame(&over).unwrap_err().code,
        RefusalCode::RequestTooLarge
    );
    // No newline within the limit: refused as too large.
    let no_newline = vec![b' '; MAX_REQUEST_BYTES];
    assert_eq!(
        parse_frame(&no_newline).unwrap_err().code,
        RefusalCode::RequestTooLarge
    );
    let no_newline_over = vec![b' '; MAX_REQUEST_BYTES + 1];
    assert_eq!(
        parse_frame(&no_newline_over).unwrap_err().code,
        RefusalCode::RequestTooLarge
    );
}

#[test]
fn framing_errors_are_malformed() {
    // The connection ended before the newline.
    assert_eq!(
        parse_frame(br#"{"protocol":1,"kind":"status"}"#)
            .unwrap_err()
            .code,
        RefusalCode::Malformed
    );
    // A second request on the same connection.
    let mut two = frame(r#"{"protocol":1,"kind":"status"}"#);
    two.extend(frame(r#"{"protocol":1,"kind":"stop"}"#));
    assert_eq!(parse_frame(&two).unwrap_err().code, RefusalCode::Malformed);
    for body in [
        "not json",
        "[1,2]",
        "\"start\"",
        "",
        r#"{"protocol":1,"kind":"status""#,
    ] {
        assert_eq!(refused(body), RefusalCode::Malformed, "{body}");
    }
    assert_eq!(
        parse_frame(b"\xff\xfe\n").unwrap_err().code,
        RefusalCode::Malformed
    );
}

#[test]
fn protocol_must_match() {
    let older = parse_frame(&frame(r#"{"protocol":0,"kind":"status"}"#)).unwrap_err();
    assert_eq!(older.code, RefusalCode::ProtocolMismatch);
    assert!(older.message.contains("upgrade the cedegrid client"));
    let newer = parse_frame(&frame(r#"{"protocol":2,"kind":"status"}"#)).unwrap_err();
    assert_eq!(newer.code, RefusalCode::ProtocolMismatch);
    assert!(newer.message.contains("cedegrid-system"));
    // A newer client's extra field still gets the version answer.
    assert_eq!(
        refused(r#"{"protocol":2,"kind":"status","priority":3}"#),
        RefusalCode::ProtocolMismatch
    );
    for body in [
        r#"{"kind":"status"}"#,
        r#"{"protocol":"1","kind":"status"}"#,
        r#"{"protocol":1.5,"kind":"status"}"#,
        r#"{"protocol":-1,"kind":"status"}"#,
    ] {
        assert_eq!(refused(body), RefusalCode::Malformed, "{body}");
    }
}

#[test]
fn unknown_kinds_and_fields_are_refused() {
    assert_eq!(
        refused(r#"{"protocol":1,"kind":"restart"}"#),
        RefusalCode::UnknownKind
    );
    assert_eq!(
        refused(r#"{"protocol":1,"kind":"START"}"#),
        RefusalCode::UnknownKind
    );
    assert_eq!(refused(r#"{"protocol":1}"#), RefusalCode::Malformed);
    assert_eq!(
        refused(r#"{"protocol":1,"kind":7}"#),
        RefusalCode::Malformed
    );
    assert_eq!(
        refused(r#"{"protocol":1,"kind":"status","verbose":true}"#),
        RefusalCode::UnknownField
    );
}

#[test]
fn identity_fields_refuse_the_whole_request() {
    for name in [
        "uid",
        "user",
        "unit",
        "path",
        "cgroup",
        "exec",
        "mount",
        "device",
        "capability",
        "UID",
        "user_id",
        "unit-name",
        "gid",
        "pid",
        "slice",
        "command",
    ] {
        let body = format!(r#"{{"protocol":1,"kind":"start","{name}":0}}"#);
        let refusal = parse_frame(&frame(&body)).unwrap_err();
        assert_eq!(refusal.code, RefusalCode::IdentityInRequest, "{name}");
        // Checked before the protocol, so an old or odd client cannot slip one past.
        let body = format!(r#"{{"protocol":9,"{name}":"x"}}"#);
        assert_eq!(refused(&body), RefusalCode::IdentityInRequest, "{name}");
    }
    // Any part of a name counts; a longer word that merely contains one does not.
    assert_eq!(
        refused(r#"{"protocol":1,"kind":"status","username_hint_x":1}"#),
        RefusalCode::IdentityInRequest
    );
    assert_eq!(
        refused(r#"{"protocol":1,"kind":"status","pathological":1}"#),
        RefusalCode::UnknownField
    );
}

#[test]
fn refusal_messages_never_echo_odd_field_names() {
    let refusal = parse_frame(&frame(r#"{"protocol":1,"kind":"status","a\nb":1}"#)).unwrap_err();
    assert_eq!(refusal.code, RefusalCode::UnknownField);
    assert!(!refusal.message.contains('\n'));
    let long = "x".repeat(200);
    let refusal = parse_frame(&frame(&format!(
        r#"{{"protocol":1,"kind":"status","{long}":1}}"#
    )))
    .unwrap_err();
    assert!(!refusal.message.contains(&long));
}

#[test]
fn job_names_are_bounded() {
    let ok = "a".repeat(128);
    assert!(
        parse_frame(&frame(&format!(
            r#"{{"protocol":1,"kind":"move","job":"{ok}","mode":"neutral"}}"#
        )))
        .is_ok()
    );
    for job in [
        "a".repeat(129),
        String::new(),
        "a/b".into(),
        "../x".into(),
        "a b".into(),
        "é".into(),
    ] {
        let body = format!(r#"{{"protocol":1,"kind":"move","job":"{job}","mode":"neutral"}}"#);
        assert_eq!(refused(&body), RefusalCode::Malformed, "{job}");
    }
    assert_eq!(
        refused(r#"{"protocol":1,"kind":"move","job":7,"mode":"neutral"}"#),
        RefusalCode::Malformed
    );
    assert_eq!(
        refused(r#"{"protocol":1,"kind":"move","mode":"neutral"}"#),
        RefusalCode::Malformed
    );
    assert_eq!(
        refused(r#"{"protocol":1,"kind":"move","job":"j"}"#),
        RefusalCode::Malformed
    );
}

#[test]
fn mode_and_job_only_where_their_kind_takes_them() {
    for kind in ["stop", "status"] {
        assert_eq!(
            refused(&format!(
                r#"{{"protocol":1,"kind":"{kind}","mode":"neutral"}}"#
            )),
            RefusalCode::UnknownField,
            "{kind}"
        );
    }
    for kind in ["start", "stop", "status"] {
        assert_eq!(
            refused(&format!(r#"{{"protocol":1,"kind":"{kind}","job":"j"}}"#)),
            RefusalCode::UnknownField,
            "{kind}"
        );
    }
    assert_eq!(
        refused(r#"{"protocol":1,"kind":"start","mode":"turbo"}"#),
        RefusalCode::Malformed
    );
    assert_eq!(
        parse_frame(&frame(
            r#"{"protocol":1,"kind":"start","mode":"aggressive"}"#
        )),
        Ok(Request::Start {
            mode: Some(ModeName::Aggressive)
        })
    );
}

#[test]
fn replies_are_one_json_line() {
    let refusal = parse_frame(&frame(r#"{"protocol":1,"kind":"restart"}"#)).unwrap_err();
    let line = Reply::refused(&refusal).to_line();
    assert!(line.ends_with('\n'));
    assert_eq!(line.matches('\n').count(), 1);
    let parsed: Reply = serde_json::from_str(line.trim_end()).unwrap();
    assert_eq!(parsed.protocol, PROTOCOL);
    assert!(!parsed.ok);
    assert_eq!(parsed.code, Some(RefusalCode::UnknownKind));
    let line = Reply::units(Vec::new()).to_line();
    assert_eq!(line, "{\"protocol\":1,\"ok\":true,\"units\":[]}\n");
}

// T007: the administrator policy.

#[test]
fn policy_defaults() {
    let policy = AdminPolicy::defaults(1000).unwrap();
    assert_eq!(policy.policy_version, 1);
    assert_eq!(policy.tasks_per_user.value, 64);
    assert_eq!(policy.tasks_max.value, 4096);
    assert_eq!(policy.uid_min.value, 1000);
    // 001's defaults, with no copy of its ranges here.
    assert_eq!(policy.system_accounts.value, SystemAccounts::default());
    assert!(policy.is_user_uid(1000) && policy.is_user_uid(60100));
    assert!(!policy.is_user_uid(999) && !policy.is_user_uid(64321));
    assert_eq!(policy.principal_min_percent.value, 1);
    assert_eq!(policy.open_files.value, None);
    assert_eq!(policy.processes.value, None);
    assert_eq!(policy.locked_memory_mib.value, None);
    assert!(policy.allows(ModeName::YieldFirst));
    assert!(policy.allows(ModeName::Neutral));
    assert!(policy.allows(ModeName::Performance));
    assert!(!policy.allows(ModeName::Aggressive));
    assert!(
        policy
            .entries()
            .iter()
            .all(|(_, _, source)| *source == Source::Default)
    );
    // The host's UID_MIN is the default for principals.uid_min.
    assert_eq!(AdminPolicy::defaults(500).unwrap().uid_min.value, 500);
    assert_eq!(AdminPolicy::parse("", 1000).unwrap(), policy);
}

/// 006 FR-016: the raise's weight is the administrator's, 400 by default, between 101 and 10000.
#[test]
fn the_raise_weight_is_the_administrators_and_bounded() {
    let policy = AdminPolicy::parse("", 1000).unwrap();
    assert_eq!(policy.unprotected_cpu_weight.value, 400);
    let policy = AdminPolicy::parse("[modes]\nunprotected_cpu_weight = 2000\n", 1000).unwrap();
    assert_eq!(policy.unprotected_cpu_weight.value, 2000);
    assert!(
        policy
            .entries()
            .iter()
            .any(|(key, value, _)| *key == "modes.unprotected_cpu_weight" && value == "2000")
    );
    for weight in [0, 100, 10_001] {
        assert!(
            AdminPolicy::parse(
                &format!("[modes]\nunprotected_cpu_weight = {weight}\n"),
                1000
            )
            .is_err(),
            "{weight}"
        );
    }
}

/// 006 FR-025: what setup says this host offers for priority over plain users. Without System
/// mode the policy is not read; a policy that cannot be read permits nothing.
#[test]
fn the_host_offers_priority_over_plain_users_where_the_policy_permits_it() {
    use cedegrid::system_mode::{PlainPriorityHost, plain_priority_host};
    assert_eq!(
        plain_priority_host(true, || AdminPolicy::parse(
            "[modes]\nplain_priority = true\nplain_priority_cpu_weight = 300\n",
            1000
        )),
        PlainPriorityHost::Permitted(300)
    );
    assert_eq!(
        plain_priority_host(true, || AdminPolicy::parse("", 1000)),
        PlainPriorityHost::NotPermitted
    );
    assert_eq!(
        plain_priority_host(false, || panic!("read without System mode")),
        PlainPriorityHost::Rootless
    );
    match plain_priority_host(true, || Err(anyhow::anyhow!("not owned by root"))) {
        PlainPriorityHost::Unreadable(reason) => assert!(reason.contains("not owned by root")),
        other => panic!("{other:?}"),
    }
}

/// 006 FR-024: priority over plain users needs the administrator's key, off by default, and its
/// weight is the administrator's, 200 by default, between 101 and 10000.
#[test]
fn priority_over_plain_users_is_the_administrators_to_permit() {
    let policy = AdminPolicy::parse("", 1000).unwrap();
    assert!(!policy.plain_priority.value);
    assert_eq!(policy.plain_priority_cpu_weight.value, 200);
    let policy = AdminPolicy::parse(
        "[modes]\nplain_priority = true\nplain_priority_cpu_weight = 300\n",
        1000,
    )
    .unwrap();
    assert!(policy.plain_priority.value);
    assert_eq!(policy.plain_priority_cpu_weight.value, 300);
    for key in ["modes.plain_priority", "modes.plain_priority_cpu_weight"] {
        assert!(policy.entries().iter().any(|(k, _, _)| *k == key), "{key}");
    }
    for weight in [0, 100, 10_001] {
        assert!(
            AdminPolicy::parse(
                &format!("[modes]\nplain_priority_cpu_weight = {weight}\n"),
                1000
            )
            .is_err(),
            "{weight}"
        );
    }
}

#[test]
fn policy_file_values_and_sources() {
    let policy = AdminPolicy::parse(
        r#"
policy_version = 1

[modes]
allowed = ["yield_first", "neutral", "aggressive"]
unprotected = true
unprotected_cpu_weight = 800
plain_priority = true
plain_priority_cpu_weight = 250

[limits]
tasks_per_user = 8
tasks_max = 512
open_files = 65536
processes = 0
locked_memory_mib = 64

[principals]
uid_min = 2000
principal_min_percent = 5

[principals.system_accounts]
uid_ranges = [[60578, 60705], [70000, 70009]]
subordinate_uid_file = ""

[units]
pam_session = "cedegrid"
ordinary_slice = "cedegrid"
"#,
        1000,
    )
    .unwrap();
    assert!(policy.allows(ModeName::Aggressive));
    assert!(!policy.allows(ModeName::Performance));
    assert!(policy.unprotected.value);
    assert_eq!(policy.tasks_per_user.value, 8);
    assert_eq!(policy.tasks_max.value, 512);
    assert_eq!(policy.open_files.value, Some(65536));
    // 0 keeps systemd's default, and still counts as set in the file.
    assert_eq!(policy.processes.value, None);
    assert_eq!(policy.processes.source, Source::File);
    assert_eq!(policy.locked_memory_mib.value, Some(64));
    assert_eq!(policy.uid_min.value, 2000);
    assert_eq!(
        policy.system_accounts.value.uid_ranges,
        [[60578, 60705], [70000, 70009]]
    );
    assert!(policy.is_user_uid(64321) && !policy.is_user_uid(70005));
    assert!(policy.entries().iter().any(|(key, value, _)| {
        *key == "principals.system_accounts.uid_ranges" && value == "60578-60705, 70000-70009"
    }));
    assert!(policy.entries().iter().any(|(key, value, _)| {
        *key == "principals.system_accounts.subordinate_uid_file" && value == "off"
    }));
    assert_eq!(policy.principal_min_percent.value, 5);
    assert!(
        policy
            .entries()
            .iter()
            .all(|(_, _, source)| *source == Source::File)
    );
}

#[test]
fn policy_refusals() {
    for text in [
        "policy_version = 2",
        "unknown = 1",
        "[modes]\nallowed = []",
        "[modes]\nallowed = [\"turbo\"]",
        "[modes]\ndenied = [\"neutral\"]",
        "[limits]\ntasks_per_user = 0",
        "[limits]\ntasks_max = 0",
        "[limits]\nopen_files = 0",
        "[limits]\ntasks_per_user = -1",
        "[limits]\nnice = 5",
        "[principals]\nuid_min = 0",
        "[principals.system_accounts]\nuid_ranges = [[2, 1]]",
        "[principals.system_accounts]\nsubordinate_uid_file = \"etc/subuid\"",
        "[principals.system_accounts]\nunknown = 1",
        "[principals]\nprincipal_min_percent = 0",
        "[principals]\nprincipal_min_percent = 51",
        "[extra]\nx = 1",
        "not toml at all [",
    ] {
        assert!(AdminPolicy::parse(text, 1000).is_err(), "{text}");
    }
}

#[test]
fn uid_min_comes_from_login_defs() {
    let text = "# UID_MIN 5\nMAIL_DIR /var/mail\nUID_MIN\t\t\t 1000\nUID_MAX 60000\n";
    assert_eq!(admin_policy::uid_min_from_login_defs(text), Some(1000));
    assert_eq!(
        admin_policy::uid_min_from_login_defs("UID_MAX 60000\n"),
        None
    );
    assert_eq!(admin_policy::uid_min_from_login_defs("UID_MIN abc\n"), None);
    assert_eq!(
        admin_policy::uid_min_from_login_defs("UID_MIN 500\nUID_MIN 700\n"),
        Some(700)
    );
}

#[cfg(unix)]
mod policy_files {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Self {
            let base = std::env::var_os("TMPDIR")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir);
            let path = base.join(format!(".system-mode-policy-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            Dir(path)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn me() -> u32 {
        // SAFETY: getuid has no preconditions and cannot fail.
        unsafe { libc::getuid() }
    }

    fn write(dir: &Dir, text: &str, mode: u32) {
        let path = dir.0.join(admin_policy::POLICY_FILE);
        std::fs::write(&path, text).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// 001 FR-059: a subordinate user ID counts as its range's owner, from a file the broker
    /// trusts as it trusts the policy. Missing, refused or turned off, the file maps nothing.
    #[test]
    fn subordinate_user_ids_count_as_their_owner_from_a_trusted_file() {
        let dir = Dir::new("subuid");
        let path = dir.0.join("subuid");
        let accounts = SystemAccounts {
            subordinate_uid_file: path.clone(),
            ..SystemAccounts::default()
        };
        let mode =
            |mode| std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        let mut file = admin_policy::SubordinateFile::default();
        file.refresh(&accounts, me()).unwrap();
        assert_eq!(file.owner(100_998), 100_998);
        std::fs::write(&path, "1234:100000:65536\n").unwrap();
        mode(0o644);
        file.refresh(&accounts, me()).unwrap();
        assert_eq!(file.owner(100_998), 1234);
        assert_eq!(file.owner(99_999), 99_999);
        // A change is read again.
        std::fs::write(&path, "1234:100000:65536\n1235:165536:65536\n").unwrap();
        file.refresh(&accounts, me()).unwrap();
        assert_eq!(file.owner(165_536), 1235);
        // Writable by others, the file is refused and maps nothing.
        mode(0o666);
        assert!(file.refresh(&accounts, me()).is_err());
        assert_eq!(file.owner(100_998), 100_998);
        mode(0o644);
        file.refresh(&accounts, me()).unwrap();
        assert_eq!(file.owner(100_998), 1234);
        // Another owner, or a symbolic link, is refused too.
        let mut other = admin_policy::SubordinateFile::default();
        assert!(other.refresh(&accounts, me() + 1).is_err());
        let link = dir.0.join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        let mut linked = admin_policy::SubordinateFile::default();
        let through_link = SystemAccounts {
            subordinate_uid_file: link,
            ..SystemAccounts::default()
        };
        assert!(linked.refresh(&through_link, me()).is_err());
        // Turned off: nothing is read or mapped.
        let off = SystemAccounts {
            subordinate_uid_file: PathBuf::new(),
            ..SystemAccounts::default()
        };
        file.refresh(&off, me()).unwrap();
        assert_eq!(file.owner(100_998), 100_998);
    }

    #[test]
    fn missing_file_gives_defaults() {
        let dir = Dir::new("missing");
        let policy = admin_policy::load_from(&dir.0, me(), 1000).unwrap();
        assert_eq!(policy, AdminPolicy::defaults(1000).unwrap());
    }

    #[test]
    fn owned_and_private_file_loads() {
        let dir = Dir::new("loads");
        write(&dir, "[limits]\ntasks_per_user = 3\n", 0o644);
        let policy = admin_policy::load_from(&dir.0, me(), 1000).unwrap();
        assert_eq!(policy.tasks_per_user.value, 3);
    }

    #[test]
    fn wrong_owner_is_refused() {
        if me() == 0 {
            return;
        }
        let dir = Dir::new("owner");
        // Root must own the directory; the test's own directory is not root's.
        let error = admin_policy::load_from(&dir.0, 0, 1000).unwrap_err();
        assert!(format!("{error:#}").contains("must be owned by user ID 0"));
    }

    #[test]
    fn writable_file_or_directory_is_refused() {
        let dir = Dir::new("writable");
        for mode in [0o664, 0o646, 0o666] {
            write(&dir, "", mode);
            let error = admin_policy::load_from(&dir.0, me(), 1000).unwrap_err();
            assert!(
                format!("{error:#}").contains("must not be writable"),
                "{mode:o}"
            );
        }
        write(&dir, "", 0o644);
        std::fs::set_permissions(&dir.0, std::fs::Permissions::from_mode(0o775)).unwrap();
        let error = admin_policy::load_from(&dir.0, me(), 1000).unwrap_err();
        assert!(format!("{error:#}").contains("must not be writable"));
    }

    #[test]
    fn symlinked_policy_is_refused() {
        let dir = Dir::new("symlink");
        let target = dir.0.join("elsewhere.toml");
        std::fs::write(&target, "").unwrap();
        std::os::unix::fs::symlink(&target, dir.0.join(admin_policy::POLICY_FILE)).unwrap();
        assert!(admin_policy::load_from(&dir.0, me(), 1000).is_err());
    }

    #[test]
    fn non_regular_policy_is_refused() {
        let dir = Dir::new("fifo");
        std::fs::create_dir(dir.0.join(admin_policy::POLICY_FILE)).unwrap();
        assert!(admin_policy::load_from(&dir.0, me(), 1000).is_err());
    }

    #[test]
    fn oversized_policy_is_refused() {
        let dir = Dir::new("big");
        write(&dir, &format!("# {}\n", "x".repeat(70 * 1024)), 0o644);
        let error = admin_policy::load_from(&dir.0, me(), 1000).unwrap_err();
        assert!(format!("{error:#}").contains("larger than"));
    }

    #[test]
    fn bad_policy_text_is_refused() {
        let dir = Dir::new("bad");
        write(&dir, "unknown = 1\n", 0o644);
        assert!(admin_policy::load_from(&dir.0, me(), 1000).is_err());
    }
}

// T008: the unit property builder.

const CALLER: Caller = Caller {
    uid: 1234,
    primary_gid: 1300,
};

fn placement(ordinary_slice: OrdinarySlice, pam: PamSession) -> Placement {
    Placement {
        ordinary_slice,
        pam,
    }
}

fn property<'a>(spec: &'a units::UnitSpec, key: &str) -> Option<&'a str> {
    spec.properties
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.as_str())
}

#[test]
fn units_are_named_and_owned_by_the_caller() {
    let policy = AdminPolicy::defaults(1000).unwrap();
    let place = placement(OrdinarySlice::UserSlice, PamSession::None);
    let ordinary = units::build(UnitKind::Ordinary, CALLER, &policy, place);
    let idle = units::build(UnitKind::Idle, CALLER, &policy, place);
    assert_eq!(ordinary.name, "cedegrid-u1234.service");
    assert_eq!(idle.name, "cedegrid-idle-u1234.service");
    for spec in [&ordinary, &idle] {
        assert_eq!(spec.uid, 1234);
        assert_eq!(spec.gid, 1300);
        assert_eq!(property(spec, "Delegate"), Some("yes"));
        assert_eq!(property(spec, "TasksMax"), Some("4096"));
        assert_eq!(property(spec, "Restart"), Some("on-failure"));
        assert!(spec.description.starts_with(units::DESCRIPTION_PREFIX));
        assert_eq!(spec.command[0], INSTALLED_BINARY);
        assert_eq!(spec.command.len(), 2);
        for forbidden in [
            "OOMScoreAdjust",
            "ManagedOOMSwap",
            "ManagedOOMMemoryPressure",
            "ExecStart",
            "User",
            "Group",
        ] {
            assert_eq!(property(spec, forbidden), None, "{forbidden}");
        }
    }
    assert_eq!(ordinary.marker, "ordinary:1234");
    assert_eq!(idle.marker, "idle:1234");
    assert_eq!(
        property(&ordinary, "RuntimeDirectory"),
        Some("cedegrid/u1234")
    );
    assert_eq!(property(&ordinary, "RuntimeDirectoryMode"), Some("0700"));
    assert_eq!(property(&idle, "RuntimeDirectory"), None);
    assert_ne!(ordinary.command, idle.command);
}

#[test]
fn ordinary_slice_follows_n4_and_idle_never_moves() {
    let policy = AdminPolicy::defaults(1000).unwrap();
    for (slice, expected) in [
        (OrdinarySlice::UserSlice, "user-1234.slice"),
        (OrdinarySlice::CedegridSlice, "cedegrid.slice"),
    ] {
        let place = placement(slice, PamSession::None);
        assert_eq!(
            units::build(UnitKind::Ordinary, CALLER, &policy, place).slice,
            expected
        );
        assert_eq!(
            units::build(UnitKind::Idle, CALLER, &policy, place).slice,
            "cedegrid_idle.slice"
        );
    }
}

#[test]
fn pam_and_limits_follow_n3() {
    let policy = AdminPolicy::parse(
        "[limits]\ntasks_max = 100\nopen_files = 4096\nprocesses = 200\nlocked_memory_mib = 2\n",
        1000,
    )
    .unwrap();
    let none = units::build(
        UnitKind::Ordinary,
        CALLER,
        &policy,
        placement(OrdinarySlice::CedegridSlice, PamSession::None),
    );
    assert_eq!(property(&none, "PAMName"), None);
    assert_eq!(property(&none, "TasksMax"), Some("100"));
    assert_eq!(property(&none, "LimitNOFILE"), Some("4096"));
    assert_eq!(property(&none, "LimitNPROC"), Some("200"));
    assert_eq!(property(&none, "LimitMEMLOCK"), Some("2097152"));
    for (pam, service) in [
        (PamSession::CedegridLimits, "cedegrid"),
        (PamSession::HostLogin, "login"),
    ] {
        let spec = units::build(
            UnitKind::Ordinary,
            CALLER,
            &policy,
            placement(OrdinarySlice::CedegridSlice, pam),
        );
        assert_eq!(property(&spec, "PAMName"), Some(service));
        // The session's limits module sets the limits; TasksMax stays the policy's.
        assert_eq!(property(&spec, "LimitNOFILE"), None);
        assert_eq!(property(&spec, "TasksMax"), Some("100"));
    }
}

#[test]
fn builder_depends_only_on_caller_policy_and_placement() {
    let policy = AdminPolicy::defaults(1000).unwrap();
    let place = placement(OrdinarySlice::UserSlice, PamSession::None);
    let first = units::build(UnitKind::Ordinary, CALLER, &policy, place);
    let second = units::build(UnitKind::Ordinary, CALLER, &policy, place);
    assert_eq!(first, second);
    let other = units::build(
        UnitKind::Ordinary,
        Caller {
            uid: 1235,
            primary_gid: 1300,
        },
        &policy,
        place,
    );
    assert_ne!(first.name, other.name);
}

#[test]
fn systemd_run_arguments_are_fixed_and_shell_free() {
    let policy = AdminPolicy::defaults(1000).unwrap();
    let spec = units::build(
        UnitKind::Ordinary,
        CALLER,
        &policy,
        placement(OrdinarySlice::UserSlice, PamSession::None),
    );
    let args: Vec<String> = systemd_run_args(&spec)
        .into_iter()
        .map(|arg| arg.into_string().unwrap())
        .collect();
    assert_eq!(
        args,
        vec![
            "--unit=cedegrid-u1234.service",
            "--slice=user-1234.slice",
            "--description=CedeGrid System mode unit (ordinary) for user ID 1234",
            "--uid=1234",
            "--gid=1300",
            "--setenv=CEDEGRID_SYSTEM_UNIT=ordinary:1234",
            "--property=Delegate=yes",
            "--property=TasksMax=4096",
            "--property=Restart=on-failure",
            "--property=RuntimeDirectory=cedegrid/u1234",
            "--property=RuntimeDirectoryMode=0700",
            "--",
            "/usr/bin/cedegrid",
            "__system-agent",
        ]
    );
}

// T042: the score function.

const TOTAL: u64 = 1_000_000;

fn process(pid: u32, start_time: u64, pages: u64, current: i32) -> ScoredProcess {
    ScoredProcess {
        key: ProcessKey { pid, start_time },
        pages,
        current,
    }
}

fn task(started: u64, mode: ModeName, processes: Vec<ScoredProcess>) -> ScoredTask {
    ScoredTask {
        started,
        mode,
        processes,
    }
}

fn input<'a>(
    tasks: &'a [ScoredTask],
    fair_share_pages: u64,
    lowering: Lowering,
    another_over_share: bool,
    marking: OverShareMarking,
) -> ScoreInput<'a> {
    ScoreInput {
        total_pages: TOTAL,
        memory: UserMemory::Known { fair_share_pages },
        another_over_share,
        lowering,
        marking,
        tasks,
    }
}

fn writes(input: &ScoreInput<'_>) -> Vec<(u32, i32)> {
    let mut writes: Vec<(u32, i32)> = score_writes(input)
        .into_iter()
        .map(|ScoreWrite { key, value }| (key.pid, value))
        .collect();
    writes.sort();
    writes
}

#[test]
fn in_share_pages_lower_the_score_by_their_part_of_the_total() {
    let tasks = [task(
        10,
        ModeName::Neutral,
        vec![process(1, 10, 250_000, 0)],
    )];
    let input = input(
        &tasks,
        400_000,
        Lowering::Always,
        false,
        OverShareMarking::EveryMode,
    );
    assert_eq!(writes(&input), vec![(1, -250)]);
}

#[test]
fn lowest_value_is_minus_999() {
    let tasks = [task(10, ModeName::Neutral, vec![process(1, 10, TOTAL, 0)])];
    let input = input(
        &tasks,
        TOTAL,
        Lowering::Always,
        false,
        OverShareMarking::EveryMode,
    );
    assert_eq!(writes(&input), vec![(1, -999)]);
}

#[test]
fn share_goes_to_the_oldest_task_first() {
    let tasks = [
        // Listed out of order on purpose.
        task(30, ModeName::Neutral, vec![process(3, 30, 300_000, 0)]),
        task(10, ModeName::Neutral, vec![process(1, 10, 300_000, 0)]),
        task(20, ModeName::Neutral, vec![process(2, 20, 300_000, 0)]),
    ];
    let input = input(
        &tasks,
        450_000,
        Lowering::Always,
        false,
        OverShareMarking::YieldFirstAndNeutral,
    );
    // Oldest gets 300k (−300), the next 150k (−150), the newest none and is marked.
    assert_eq!(writes(&input), vec![(1, -300), (2, -150), (3, 1000)]);
}

#[test]
fn marking_follows_the_mode_and_d2() {
    let tasks = [
        task(10, ModeName::Performance, vec![process(1, 10, 100_000, 0)]),
        task(20, ModeName::Performance, vec![process(2, 20, 100_000, 0)]),
        task(30, ModeName::YieldFirst, vec![process(3, 30, 100_000, 0)]),
    ];
    // D2 case A: only Yield First and Neutral mark.
    let case_a = input(
        &tasks,
        100_000,
        Lowering::Never,
        false,
        OverShareMarking::YieldFirstAndNeutral,
    );
    assert_eq!(writes(&case_a), vec![(3, 1000)]);
    // D2 case B: every mode marks.
    let case_b = input(
        &tasks,
        100_000,
        Lowering::Never,
        false,
        OverShareMarking::EveryMode,
    );
    assert_eq!(writes(&case_b), vec![(2, 1000), (3, 1000)]);
}

#[test]
fn a_task_partly_in_share_is_not_marked() {
    let tasks = [task(
        10,
        ModeName::Neutral,
        vec![process(1, 10, 100_000, 0), process(2, 11, 100_000, 0)],
    )];
    let input = input(
        &tasks,
        150_000,
        Lowering::Never,
        false,
        OverShareMarking::EveryMode,
    );
    assert_eq!(writes(&input), Vec::<(u32, i32)>::new());
}

#[test]
fn lowering_follows_n1() {
    let tasks = [task(
        10,
        ModeName::Neutral,
        vec![process(1, 10, 200_000, 0)],
    )];
    for (lowering, another_over, expected) in [
        (Lowering::Always, false, vec![(1, -200)]),
        (Lowering::Always, true, vec![(1, -200)]),
        (Lowering::WhileAnotherOverShare, true, vec![(1, -200)]),
        (Lowering::WhileAnotherOverShare, false, vec![]),
        (Lowering::Never, true, vec![]),
        (Lowering::Never, false, vec![]),
    ] {
        let input = input(
            &tasks,
            500_000,
            lowering,
            another_over,
            OverShareMarking::EveryMode,
        );
        assert_eq!(writes(&input), expected, "{lowering:?} {another_over}");
    }
}

#[test]
fn lowered_scores_return_when_lowering_stops() {
    let tasks = [task(
        10,
        ModeName::Neutral,
        vec![process(1, 10, 200_000, -200)],
    )];
    let input = input(
        &tasks,
        500_000,
        Lowering::WhileAnotherOverShare,
        false,
        OverShareMarking::EveryMode,
    );
    assert_eq!(writes(&input), vec![(1, 0)]);
}

#[test]
fn unchanged_values_are_not_written() {
    let tasks = [task(
        10,
        ModeName::Neutral,
        vec![process(1, 10, 250_000, -250), process(2, 11, 0, 0)],
    )];
    let input = input(
        &tasks,
        400_000,
        Lowering::Always,
        false,
        OverShareMarking::EveryMode,
    );
    assert_eq!(writes(&input), Vec::<(u32, i32)>::new());
}

#[test]
fn unknown_memory_lowers_nothing_and_returns_after_the_window() {
    let tasks = [task(
        10,
        ModeName::Neutral,
        vec![process(1, 10, 250_000, -250), process(2, 11, 1, 1000)],
    )];
    let mut input = input(
        &tasks,
        0,
        Lowering::Always,
        true,
        OverShareMarking::EveryMode,
    );
    input.memory = UserMemory::Unknown {
        for_how_long: Duration::from_secs(3),
    };
    assert_eq!(writes(&input), Vec::<(u32, i32)>::new());
    input.memory = UserMemory::Unknown {
        for_how_long: UNKNOWN_WINDOW,
    };
    assert_eq!(writes(&input), vec![(1, 0), (2, 0)]);
}

#[test]
fn an_empty_task_is_never_marked() {
    let tasks = [task(10, ModeName::Neutral, vec![process(1, 10, 0, 0)])];
    let input = input(
        &tasks,
        0,
        Lowering::Always,
        true,
        OverShareMarking::EveryMode,
    );
    assert_eq!(writes(&input), Vec::<(u32, i32)>::new());
}

// T009: the process handle the broker's writes go through (Linux only).

#[cfg(target_os = "linux")]
mod process_handles {
    use cedegrid::broker::process_handle::{
        ProcessHandle, parse_cgroup_v2, parse_ids, parse_start_time,
    };
    use std::process::{Child, Command};

    fn sleeper() -> Child {
        Command::new("sleep").arg("30").spawn().unwrap()
    }

    #[test]
    fn holds_a_live_process_and_reads_it() {
        let mut child = sleeper();
        let handle = ProcessHandle::open(child.id(), None).unwrap();
        assert_eq!(handle.pid(), child.id());
        assert!(handle.start_time() > 0);
        assert!(handle.is_alive().unwrap());
        // SAFETY: getuid and getgid have no preconditions and cannot fail.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        let ids = handle.ids().unwrap();
        assert!(ids.all_are(uid, gid), "{ids:?}");
        assert!(!ids.all_are(uid.wrapping_add(1), gid));
        assert!(handle.cgroup().unwrap().starts_with('/'));
        let again = ProcessHandle::open(child.id(), Some(handle.start_time())).unwrap();
        assert_eq!(again.start_time(), handle.start_time());
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn another_start_time_is_refused() {
        let mut child = sleeper();
        let handle = ProcessHandle::open(child.id(), None).unwrap();
        let error = ProcessHandle::open(child.id(), Some(handle.start_time() + 1)).unwrap_err();
        assert!(format!("{error:#}").contains("reused"));
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn writes_are_read_back() {
        let mut child = sleeper();
        let handle = ProcessHandle::open(child.id(), None).unwrap();
        // Raising a process's own score needs no privilege.
        handle.set_oom_score_adj(500).unwrap();
        assert_eq!(handle.oom_score_adj().unwrap(), 500);
        handle.set_oom_score_adj(300).unwrap();
        assert_eq!(handle.oom_score_adj().unwrap(), 300);
        assert!(handle.set_oom_score_adj(1001).is_err());
        assert!(handle.set_oom_score_adj(-1001).is_err());
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn an_exited_process_fails_closed() {
        let mut child = sleeper();
        let pid = child.id();
        let handle = ProcessHandle::open(pid, None).unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!handle.is_alive().unwrap());
        // The held directory reaches nothing once its process is gone, whatever holds the PID.
        assert!(handle.set_oom_score_adj(400).is_err());
        assert!(handle.oom_score_adj().is_err());
        assert!(handle.ids().is_err());
        assert!(ProcessHandle::open(pid, None).is_err());
        assert!(ProcessHandle::open(0, None).is_err());
    }

    #[test]
    fn proc_text_parsers_on_linux() {
        let stat =
            "1234 (a) b (c)) S 1 1234 1234 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 98765 1000 50";
        assert_eq!(parse_start_time(stat), Some(98765));
        assert_eq!(parse_start_time("1234 (short) S 1"), None);
        let status = "Name:\tx\nUid:\t1000\t1000\t1000\t1000\nGid:\t100\t100\t100\t101\n";
        let ids = parse_ids(status).unwrap();
        assert_eq!(ids.uids, [1000; 4]);
        assert_eq!(ids.gids, [100, 100, 100, 101]);
        assert!(!ids.all_are(1000, 100));
        assert_eq!(parse_ids("Uid:\t1\t1\t1\n"), None);
        assert_eq!(
            parse_cgroup_v2("12:cpu:/old\n0::/system.slice/cedegrid-u1000.service/job\n"),
            Some("/system.slice/cedegrid-u1000.service/job".to_owned())
        );
        assert_eq!(parse_cgroup_v2("12:cpu:/old\n"), None);
    }
}

// T010 (decision part) and T024: the root refusal and the client.

mod client {
    use cedegrid::broker::BROKER_SUBCOMMAND;
    use cedegrid::broker::ModeName;
    use cedegrid::broker::request::{JobId, Refusal, RefusalCode, Request, parse_frame};
    use cedegrid::system_mode::{Decision, Outcome, decide, reply_outcome, root_refusal};
    use std::ffi::OsStr;

    #[test]
    fn root_is_refused_except_for_the_broker() {
        let line = root_refusal(0, None).unwrap();
        assert!(line.contains("without sudo"));
        assert!(root_refusal(0, Some(OsStr::new("agent"))).is_some());
        assert!(root_refusal(0, Some(OsStr::new("__worker-gate"))).is_some());
        assert_eq!(root_refusal(0, Some(OsStr::new(BROKER_SUBCOMMAND))), None);
        assert_eq!(root_refusal(1000, None), None);
        assert_eq!(root_refusal(1000, Some(OsStr::new("agent"))), None);
    }

    #[test]
    fn client_lines_parse_as_the_broker_reads_them() {
        for request in [
            Request::Start { mode: None },
            Request::Start {
                mode: Some(ModeName::YieldFirst),
            },
            Request::Stop,
            Request::Status,
            Request::Move {
                job: JobId::parse("j-1").unwrap(),
                mode: ModeName::Performance,
            },
        ] {
            assert_eq!(
                parse_frame(request.to_line().as_bytes()),
                Ok(request.clone()),
                "{request:?}"
            );
        }
    }

    fn refused(code: RefusalCode) -> Outcome {
        Outcome::Refused(Refusal::new(code, "Reason."))
    }

    #[test]
    fn fallback_and_refusals() {
        assert_eq!(
            decide(Outcome::NotInstalled, false),
            Decision::Fallback(None)
        );
        assert!(matches!(
            decide(Outcome::NotInstalled, true),
            Decision::Refuse(line) if line.contains("not installed")
        ));
        assert_eq!(
            decide(Outcome::NoAnswer("is not running".into()), false),
            Decision::Fallback(Some(
                "System mode unavailable, continuing without it: broker is not running".into()
            ))
        );
        assert!(matches!(
            decide(Outcome::NoAnswer("is not running".into()), true),
            Decision::Refuse(line) if line.starts_with("System mode required (--require-system)")
        ));
        for code in [
            RefusalCode::NotAMember,
            RefusalCode::RateLimited,
            RefusalCode::SystemdFailed,
            RefusalCode::SystemdTimeout,
            RefusalCode::ProtocolMismatch,
        ] {
            assert!(
                matches!(decide(refused(code), false), Decision::Fallback(Some(_))),
                "{code:?}"
            );
            assert!(
                matches!(decide(refused(code), true), Decision::Refuse(_)),
                "{code:?}"
            );
        }
        // The administrator's policy is never bypassed by falling back.
        for code in [
            RefusalCode::ModeNotAllowed,
            RefusalCode::TaskLimit,
            RefusalCode::UnitNotOurs,
        ] {
            assert_eq!(
                decide(refused(code), false),
                Decision::Refuse("Reason.".into()),
                "{code:?}"
            );
        }
        assert_eq!(
            decide(Outcome::Units(Vec::new()), true),
            Decision::System(Vec::new())
        );
    }

    #[test]
    fn replies_from_another_protocol_or_garbage() {
        assert_eq!(
            reply_outcome(b"{\"protocol\":1,\"ok\":true,\"units\":[]}\n"),
            Outcome::Units(Vec::new())
        );
        assert_eq!(
            reply_outcome(b"{\"protocol\":1,\"ok\":true}\n"),
            Outcome::Units(Vec::new())
        );
        match reply_outcome(b"{\"protocol\":2,\"ok\":false,\"code\":\"protocol_mismatch\"}\n") {
            Outcome::Refused(refusal) => {
                assert_eq!(refusal.code, RefusalCode::ProtocolMismatch);
                assert!(refusal.message.contains("upgrade the cedegrid client"));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            reply_outcome(
                b"{\"protocol\":1,\"ok\":false,\"code\":\"not_a_member\",\"message\":\"Join.\"}\n"
            ),
            Outcome::Refused(Refusal::new(RefusalCode::NotAMember, "Join."))
        );
        for garbage in [
            &b"nonsense\n"[..],
            b"{\"protocol\":1,\"ok\":true,\"code\":\"malformed\"}\n",
            b"{\"protocol\":1,\"ok\":false}\n",
            b"{\"protocol\":1,\"ok\":true,\"extra\":1}\n",
        ] {
            assert!(
                matches!(reply_outcome(garbage), Outcome::NoAnswer(_)),
                "{}",
                String::from_utf8_lossy(garbage)
            );
        }
    }

    #[cfg(unix)]
    mod socket {
        use super::*;
        use cedegrid::system_mode::ask;
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;
        use std::path::PathBuf;
        use std::time::{Duration, Instant};

        struct Dir(PathBuf);

        impl Dir {
            fn new(name: &str) -> Self {
                let base = std::env::var_os("TMPDIR")
                    .map(PathBuf::from)
                    .unwrap_or_else(std::env::temp_dir);
                let path = base.join(format!(".sm-{name}-{}", std::process::id()));
                let _ = std::fs::remove_dir_all(&path);
                std::fs::create_dir_all(&path).unwrap();
                Dir(path)
            }

            fn socket(&self) -> PathBuf {
                self.0.join("b.sock")
            }
        }

        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        /// A broker stand-in that reads one line and answers with `reply`, or never answers.
        fn serve(
            listener: UnixListener,
            reply: Option<&'static str>,
        ) -> std::thread::JoinHandle<String> {
            std::thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                let mut line = String::new();
                BufReader::new(&stream).read_line(&mut line).unwrap();
                match reply {
                    Some(reply) => (&stream).write_all(reply.as_bytes()).unwrap(),
                    None => std::thread::sleep(Duration::from_millis(600)),
                }
                line
            })
        }

        const SHORT: Duration = Duration::from_millis(300);

        #[test]
        fn no_socket_is_not_installed() {
            let dir = Dir::new("none");
            assert_eq!(
                ask(&dir.socket(), &Request::Status, SHORT),
                Outcome::NotInstalled
            );
        }

        #[test]
        fn a_plain_file_or_a_dead_socket_is_no_answer() {
            let dir = Dir::new("dead");
            std::fs::write(dir.socket(), "").unwrap();
            assert!(matches!(
                ask(&dir.socket(), &Request::Status, SHORT),
                Outcome::NoAnswer(_)
            ));
            std::fs::remove_file(dir.socket()).unwrap();
            drop(UnixListener::bind(dir.socket()).unwrap());
            assert_eq!(
                ask(&dir.socket(), &Request::Status, SHORT),
                Outcome::NoAnswer("is not running".into())
            );
        }

        #[test]
        fn a_served_request_returns_units() {
            let dir = Dir::new("served");
            let listener = UnixListener::bind(dir.socket()).unwrap();
            let server = serve(
                listener,
                Some(
                    "{\"protocol\":1,\"ok\":true,\"units\":[{\"kind\":\"ordinary\",\"name\":\"cedegrid-u1000.service\",\"state\":\"active\",\"cgroup\":\"/user.slice/user-1000.slice/cedegrid-u1000.service\"}]}\n",
                ),
            );
            let request = Request::Start {
                mode: Some(ModeName::Neutral),
            };
            match ask(&dir.socket(), &request, Duration::from_secs(2)) {
                Outcome::Units(units) => {
                    assert_eq!(units.len(), 1);
                    assert_eq!(units[0].name, "cedegrid-u1000.service");
                }
                other => panic!("{other:?}"),
            }
            assert_eq!(server.join().unwrap(), request.to_line());
        }

        #[test]
        fn a_silent_broker_is_given_up_on_in_time() {
            let dir = Dir::new("silent");
            let listener = UnixListener::bind(dir.socket()).unwrap();
            let server = serve(listener, None);
            let started = Instant::now();
            let outcome = ask(&dir.socket(), &Request::Status, SHORT);
            assert!(
                started.elapsed() < Duration::from_millis(550),
                "{:?}",
                started.elapsed()
            );
            assert!(
                matches!(outcome, Outcome::NoAnswer(reason) if reason.contains("did not answer"))
            );
            server.join().unwrap();
        }

        #[test]
        fn a_refusal_comes_through() {
            let dir = Dir::new("refused");
            let listener = UnixListener::bind(dir.socket()).unwrap();
            let server = serve(
                listener,
                Some(
                    "{\"protocol\":1,\"ok\":false,\"code\":\"mode_not_allowed\",\"message\":\"The policy allows only neutral.\"}\n",
                ),
            );
            assert_eq!(
                ask(
                    &dir.socket(),
                    &Request::Start {
                        mode: Some(ModeName::Performance)
                    },
                    SHORT
                ),
                Outcome::Refused(Refusal::new(
                    RefusalCode::ModeNotAllowed,
                    "The policy allows only neutral."
                ))
            );
            server.join().unwrap();
        }

        #[test]
        fn a_closed_connection_is_no_answer() {
            let dir = Dir::new("closed");
            let listener = UnixListener::bind(dir.socket()).unwrap();
            let server = serve(listener, Some(""));
            assert!(matches!(
                ask(&dir.socket(), &Request::Status, SHORT),
                Outcome::NoAnswer(reason) if reason.contains("without a reply")
            ));
            server.join().unwrap();
        }
    }
}

// T019 to T021, T032, T034: the peer check, the systemd calls and the listener, against fakes.

#[cfg(unix)]
mod broker_service {
    use cedegrid::broker::ModeName;
    use cedegrid::broker::admin_policy::AdminPolicy;
    use cedegrid::broker::peer::{Accounts, PeerCredentials, authorize};
    use cedegrid::broker::request::{MAX_REQUEST_BYTES, Refusal, RefusalCode, Reply, UnitStatus};
    use cedegrid::broker::server::{
        BrokerState, RateLimiter, handle, read_frame, serve, serve_connection,
    };
    use cedegrid::broker::systemd::{
        Systemd, UnitShow, parse_show, start_units, status_units, stop_units,
    };
    use cedegrid::broker::units::{
        Caller, MARKER_VARIABLE, OrdinarySlice, PamSession, Placement, UnitKind, UnitSpec,
        marker_value,
    };
    use std::collections::{BTreeMap, BTreeSet};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    const PLACE: Placement = Placement {
        ordinary_slice: OrdinarySlice::UserSlice,
        pam: PamSession::None,
    };

    struct FakeAccounts {
        members: BTreeSet<u32>,
        broken: bool,
    }

    impl Accounts for FakeAccounts {
        fn primary_gid(&self, uid: u32) -> anyhow::Result<u32> {
            anyhow::ensure!(!self.broken, "database unreadable");
            Ok(uid + 100)
        }

        fn is_member(&self, uid: u32) -> anyhow::Result<bool> {
            anyhow::ensure!(!self.broken, "database unreadable");
            Ok(self.members.contains(&uid))
        }
    }

    fn accounts(members: &[u32]) -> FakeAccounts {
        FakeAccounts {
            members: members.iter().copied().collect(),
            broken: false,
        }
    }

    /// A systemd stand-in that records calls and keeps unit state.
    #[derive(Default)]
    struct FakeSystemd {
        units: Mutex<BTreeMap<String, UnitShow>>,
        /// Slices' CPU weights; a missing slice is at the default.
        weights: Mutex<BTreeMap<String, u64>>,
        calls: Mutex<Vec<String>>,
        fail_start: bool,
        /// A stopped unit is left failed, as an agent that exits non-zero on TERM leaves it.
        stop_leaves_failed: bool,
    }

    impl FakeSystemd {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn put(&self, name: &str, show: UnitShow) {
            self.units.lock().unwrap().insert(name.to_owned(), show);
        }
    }

    fn ours(kind: UnitKind, uid: u32, state: &str) -> UnitShow {
        UnitShow {
            load_state: "loaded".into(),
            active_state: state.into(),
            control_group: format!("/cg/{uid}"),
            description: "CedeGrid System mode unit (x) for user ID".into(),
            environment: vec![format!("{MARKER_VARIABLE}={}", marker_value(kind, uid))],
        }
    }

    impl Systemd for FakeSystemd {
        fn show(&self, unit: &str) -> Result<UnitShow, Refusal> {
            self.calls.lock().unwrap().push(format!("show {unit}"));
            Ok(self
                .units
                .lock()
                .unwrap()
                .get(unit)
                .cloned()
                .unwrap_or(UnitShow {
                    load_state: "not-found".into(),
                    active_state: "inactive".into(),
                    ..UnitShow::default()
                }))
        }

        fn start(&self, spec: &UnitSpec) -> Result<(), Refusal> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("start {} {}", spec.name, spec.slice));
            if self.fail_start {
                return Err(Refusal::new(RefusalCode::SystemdFailed, "failed."));
            }
            self.put(&spec.name, ours(spec.kind, spec.uid, "active"));
            Ok(())
        }

        fn stop(&self, unit: &str) -> Result<(), Refusal> {
            self.calls.lock().unwrap().push(format!("stop {unit}"));
            let mut units = self.units.lock().unwrap();
            if self.stop_leaves_failed {
                if let Some(show) = units.get_mut(unit) {
                    show.active_state = "failed".into();
                }
            } else {
                units.remove(unit);
            }
            Ok(())
        }

        fn reset_failed(&self, unit: &str) -> Result<(), Refusal> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("reset-failed {unit}"));
            self.units.lock().unwrap().remove(unit);
            Ok(())
        }

        fn slice_weight(&self, slice: &str) -> Result<Option<u64>, Refusal> {
            Ok(self.weights.lock().unwrap().get(slice).copied())
        }

        fn set_slice_weight(&self, slice: &str, weight: Option<u64>) -> Result<(), Refusal> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("weight {slice} {weight:?}"));
            let mut weights = self.weights.lock().unwrap();
            match weight {
                Some(weight) => weights.insert(slice.to_owned(), weight),
                None => weights.remove(slice),
            };
            Ok(())
        }
    }

    /// Lets a test keep a handle on the fake after the broker state takes it.
    struct Shared(Arc<FakeSystemd>);

    impl Systemd for Shared {
        fn show(&self, unit: &str) -> Result<UnitShow, Refusal> {
            self.0.show(unit)
        }

        fn start(&self, spec: &UnitSpec) -> Result<(), Refusal> {
            self.0.start(spec)
        }

        fn stop(&self, unit: &str) -> Result<(), Refusal> {
            self.0.stop(unit)
        }

        fn reset_failed(&self, unit: &str) -> Result<(), Refusal> {
            self.0.reset_failed(unit)
        }

        fn slice_weight(&self, slice: &str) -> Result<Option<u64>, Refusal> {
            self.0.slice_weight(slice)
        }

        fn set_slice_weight(&self, slice: &str, weight: Option<u64>) -> Result<(), Refusal> {
            self.0.set_slice_weight(slice, weight)
        }
    }

    /// Cgroup files as a map, for the idle slice's `cpu.idle`; `frozen` refuses writes.
    #[derive(Default)]
    struct MemCgroups {
        files: Mutex<BTreeMap<String, String>>,
        read_only: bool,
    }

    impl cedegrid::broker::moves::CgroupFs for MemCgroups {
        fn read(&self, cgroup: &str, file: &str) -> anyhow::Result<String> {
            self.files
                .lock()
                .unwrap()
                .get(&format!("{cgroup}/{file}"))
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no such file"))
        }
        fn write(&self, cgroup: &str, file: &str, value: &str) -> anyhow::Result<()> {
            anyhow::ensure!(!self.read_only, "read-only");
            self.files
                .lock()
                .unwrap()
                .insert(format!("{cgroup}/{file}"), value.to_owned());
            Ok(())
        }
        fn children(&self, _: &str) -> anyhow::Result<Vec<String>> {
            Ok(Vec::new())
        }
    }

    struct NoProcesses;

    impl cedegrid::broker::oom::ProcessAccess for NoProcesses {
        fn open(
            &self,
            _: u32,
            _: Option<u64>,
        ) -> anyhow::Result<Box<dyn cedegrid::broker::oom::HeldProcess>> {
            anyhow::bail!("no processes in this test")
        }
    }

    const CALLER: Caller = Caller {
        uid: 1234,
        primary_gid: 1334,
    };

    fn peer(uid: u32) -> PeerCredentials {
        PeerCredentials {
            uid,
            gid: uid,
            pid: Some(42),
        }
    }

    #[test]
    fn callers_are_checked_against_the_group_now() {
        let members = accounts(&[1234]);
        assert_eq!(authorize(peer(1234), &members, 1000), Ok(CALLER));
        assert_eq!(
            authorize(peer(1235), &members, 1000).unwrap_err().code,
            RefusalCode::NotAMember
        );
        assert_eq!(
            authorize(peer(0), &accounts(&[0]), 0).unwrap_err().code,
            RefusalCode::RefusedAccount
        );
        assert_eq!(
            authorize(peer(999), &accounts(&[999]), 1000)
                .unwrap_err()
                .code,
            RefusalCode::RefusedAccount
        );
        let broken = FakeAccounts {
            members: [1234].into(),
            broken: true,
        };
        assert_eq!(
            authorize(peer(1234), &broken, 1000).unwrap_err().code,
            RefusalCode::NotAMember
        );
    }

    #[test]
    fn show_output_parses_and_the_marker_is_checked() {
        let show = parse_show(
            "LoadState=loaded\nActiveState=active\nControlGroup=/system.slice/x\nDescription=CedeGrid System mode unit (ordinary) for user ID 1234\nEnvironment=A=b CEDEGRID_SYSTEM_UNIT=ordinary:1234\nnoise\n",
        );
        assert_eq!(show.load_state, "loaded");
        assert_eq!(show.active_state, "active");
        assert_eq!(show.control_group, "/system.slice/x");
        assert!(!show.absent());
        assert!(show.is_ours(UnitKind::Ordinary, 1234));
        assert!(!show.is_ours(UnitKind::Idle, 1234));
        assert!(!show.is_ours(UnitKind::Ordinary, 1235));
        let mut other = show.clone();
        other.description = "Something else".into();
        assert!(!other.is_ours(UnitKind::Ordinary, 1234));
        assert!(parse_show("LoadState=not-found\n").absent());
    }

    #[test]
    fn start_makes_both_units_once() {
        let systemd = FakeSystemd::default();
        let policy = AdminPolicy::defaults(1000).unwrap();
        let units = start_units(&systemd, CALLER, &policy, PLACE).unwrap();
        assert_eq!(units.len(), 2);
        assert!(units.iter().all(|unit| unit.state == "active"));
        let starts = |systemd: &FakeSystemd| {
            systemd
                .calls()
                .iter()
                .filter(|call| call.starts_with("start "))
                .count()
        };
        assert_eq!(starts(&systemd), 2);
        // A second start leaves running units alone.
        start_units(&systemd, CALLER, &policy, PLACE).unwrap();
        assert_eq!(starts(&systemd), 2);
    }

    #[test]
    fn a_failed_unit_of_ours_is_reset_then_started() {
        let systemd = FakeSystemd::default();
        systemd.put(
            "cedegrid-u1234.service",
            ours(UnitKind::Ordinary, 1234, "failed"),
        );
        let policy = AdminPolicy::defaults(1000).unwrap();
        start_units(&systemd, CALLER, &policy, PLACE).unwrap();
        let calls = systemd.calls();
        let reset = calls
            .iter()
            .position(|call| call == "reset-failed cedegrid-u1234.service")
            .unwrap();
        let start = calls
            .iter()
            .position(|call| call.starts_with("start cedegrid-u1234.service "))
            .unwrap();
        assert!(reset < start);
    }

    #[test]
    fn a_unit_without_the_marker_is_never_touched() {
        let foreign = UnitShow {
            load_state: "loaded".into(),
            active_state: "active".into(),
            description: "An administrator's own unit".into(),
            ..UnitShow::default()
        };
        let policy = AdminPolicy::defaults(1000).unwrap();
        for name in ["cedegrid-u1234.service", "cedegrid-idle-u1234.service"] {
            let systemd = FakeSystemd::default();
            systemd.put(name, foreign.clone());
            assert_eq!(
                start_units(&systemd, CALLER, &policy, PLACE)
                    .unwrap_err()
                    .code,
                RefusalCode::UnitNotOurs
            );
            assert_eq!(
                stop_units(&systemd, CALLER).unwrap_err().code,
                RefusalCode::UnitNotOurs
            );
            assert_eq!(
                status_units(&systemd, CALLER).unwrap_err().code,
                RefusalCode::UnitNotOurs
            );
            assert!(
                !systemd
                    .calls()
                    .iter()
                    .any(|call| call.starts_with("stop ") || call.starts_with("reset-failed ")),
                "{name}"
            );
        }
    }

    #[test]
    fn stop_and_status_reach_only_the_callers_units() {
        let systemd = FakeSystemd::default();
        let policy = AdminPolicy::defaults(1000).unwrap();
        start_units(&systemd, CALLER, &policy, PLACE).unwrap();
        let other = Caller {
            uid: 2000,
            primary_gid: 2100,
        };
        start_units(&systemd, other, &policy, PLACE).unwrap();
        let stopped = stop_units(&systemd, CALLER).unwrap();
        assert!(stopped.iter().all(|unit| unit.state == "absent"));
        let others = status_units(&systemd, other).unwrap();
        assert!(others.iter().all(|unit| unit.state == "active"));
        assert!(
            systemd
                .calls()
                .iter()
                .filter(|call| call.starts_with("stop "))
                .all(|call| call.ends_with("u1234.service"))
        );
    }

    #[test]
    fn a_unit_left_failed_by_a_stop_is_cleared() {
        let systemd = FakeSystemd {
            stop_leaves_failed: true,
            ..FakeSystemd::default()
        };
        let policy = AdminPolicy::defaults(1000).unwrap();
        start_units(&systemd, CALLER, &policy, PLACE).unwrap();
        let stopped = stop_units(&systemd, CALLER).unwrap();
        assert!(stopped.iter().all(|unit| unit.state == "absent"));
        assert_eq!(
            systemd
                .calls()
                .iter()
                .filter(|call| call.starts_with("reset-failed "))
                .count(),
            2
        );
    }

    fn state(systemd: FakeSystemd, members: &[u32], policy: Option<&'static str>) -> BrokerState {
        BrokerState {
            systemd: Box::new(systemd),
            accounts: Box::new(accounts(members)),
            load_policy: Box::new(move || AdminPolicy::parse(policy.unwrap_or(""), 1000)),
            host_uid_min: 1000,
            user_slice_splits_cpu: Box::new(|| true),
            cgroups: Box::new(MemCgroups::default()),
            processes: Box::new(NoProcesses),
            rate: Mutex::new(RateLimiter::new(8, 2)),
            callers: Mutex::new(Default::default()),
            raises: Mutex::new(Default::default()),
        }
    }

    fn line(json: &str) -> Vec<u8> {
        format!("{json}\n").into_bytes()
    }

    /// 006 FR-016, FR-018: the raise reaches only the caller's account slice, only while the
    /// administrator permits unprotected and the caller's ordinary unit sits in that slice; it is
    /// read back, kept for a lease the caller renews, and reset on withdrawal, on the lease's
    /// expiry and on stop.
    #[test]
    fn the_raise_weights_only_the_callers_account_slice_while_permitted() {
        use cedegrid::broker::server::expire_raises;
        let in_slice = |systemd: &FakeSystemd| {
            for kind in [UnitKind::Ordinary, UnitKind::Idle] {
                let mut show = ours(kind, 1234, "active");
                show.control_group = format!(
                    "/user.slice/user-1234.slice/{}",
                    cedegrid::broker::units::unit_name(kind, 1234)
                );
                systemd.put(&cedegrid::broker::units::unit_name(kind, 1234), show);
            }
        };
        let raise = |on: bool| line(&format!(r#"{{"protocol":1,"kind":"raise","on":{on}}}"#));
        // Not permitted by the policy: refused, nothing written.
        let systemd = Arc::new(FakeSystemd::default());
        in_slice(&systemd);
        let mut denied = state(FakeSystemd::default(), &[1234], None);
        denied.systemd = Box::new(Shared(Arc::clone(&systemd)));
        let handled = handle(&denied, peer(1234), &raise(true));
        assert_eq!(handled.reply.code, Some(RefusalCode::RaiseRefused));
        assert!(systemd.weights.lock().unwrap().is_empty());
        // Permitted, but the ordinary unit sits in cedegrid.slice: no raise reaches others there.
        let permitted = "[modes]\nunprotected = true\nunprotected_cpu_weight = 500\n";
        let elsewhere = Arc::new(FakeSystemd::default());
        elsewhere.put(
            "cedegrid-u1234.service",
            ours(UnitKind::Ordinary, 1234, "active"),
        );
        elsewhere.put(
            "cedegrid-idle-u1234.service",
            ours(UnitKind::Idle, 1234, "active"),
        );
        let mut state_elsewhere = state(FakeSystemd::default(), &[1234], Some(permitted));
        state_elsewhere.systemd = Box::new(Shared(Arc::clone(&elsewhere)));
        let handled = handle(&state_elsewhere, peer(1234), &raise(true));
        assert_eq!(handled.reply.code, Some(RefusalCode::RaiseRefused));
        assert!(elsewhere.weights.lock().unwrap().is_empty());
        // Permitted and in the account slice: that slice, and only it, at the policy's weight.
        let mut granted = state(FakeSystemd::default(), &[1234, 1235], Some(permitted));
        granted.systemd = Box::new(Shared(Arc::clone(&systemd)));
        let handled = handle(&granted, peer(1234), &raise(true));
        assert!(handled.reply.ok, "{:?}", handled.reply);
        assert_eq!(
            *systemd.weights.lock().unwrap(),
            BTreeMap::from([("user-1234.slice".to_owned(), 500)])
        );
        // A lease not renewed in time ends, and the slice is back at its default.
        let now = std::time::Instant::now();
        expire_raises(&granted, now);
        assert_eq!(systemd.weights.lock().unwrap().len(), 1);
        expire_raises(&granted, now + std::time::Duration::from_secs(31));
        assert!(systemd.weights.lock().unwrap().is_empty());
        // Withdrawal resets at once, permitted or not.
        assert!(handle(&granted, peer(1234), &raise(true)).reply.ok);
        assert!(
            handle(&denied_after(&systemd), peer(1234), &raise(false))
                .reply
                .ok
        );
        assert!(systemd.weights.lock().unwrap().is_empty());
        // Stopping the units resets it too.
        assert!(handle(&granted, peer(1234), &raise(true)).reply.ok);
        assert!(
            handle(
                &granted,
                peer(1234),
                &line(r#"{"protocol":1,"kind":"stop"}"#)
            )
            .reply
            .ok
        );
        assert!(systemd.weights.lock().unwrap().is_empty());
        // Another member's request never reaches this slice.
        in_slice(&systemd);
        let handled = handle(&granted, peer(1235), &raise(true));
        assert_eq!(handled.reply.code, Some(RefusalCode::RaiseRefused));
        assert!(systemd.weights.lock().unwrap().is_empty());
    }

    /// 006 FR-016: a raise never replaces a weight someone else gave the account slice. The
    /// policy's own weight, as a broker that stopped leaves it, is taken over and renewed.
    #[test]
    fn a_raise_never_replaces_another_weight_on_the_account_slice() {
        let permitted = "[modes]\nunprotected = true\nunprotected_cpu_weight = 500\n";
        let systemd = Arc::new(FakeSystemd::default());
        for kind in [UnitKind::Ordinary, UnitKind::Idle] {
            let mut show = ours(kind, 1234, "active");
            show.control_group = format!(
                "/user.slice/user-1234.slice/{}",
                cedegrid::broker::units::unit_name(kind, 1234)
            );
            systemd.put(&cedegrid::broker::units::unit_name(kind, 1234), show);
        }
        let mut granted = state(FakeSystemd::default(), &[1234], Some(permitted));
        granted.systemd = Box::new(Shared(Arc::clone(&systemd)));
        let raise = line(r#"{"protocol":1,"kind":"raise","on":true}"#);
        systemd
            .weights
            .lock()
            .unwrap()
            .insert("user-1234.slice".into(), 250);
        let handled = handle(&granted, peer(1234), &raise);
        assert_eq!(handled.reply.code, Some(RefusalCode::RaiseRefused));
        assert!(
            handled
                .reply
                .message
                .as_deref()
                .unwrap_or("")
                .contains("250"),
            "{:?}",
            handled.reply
        );
        assert_eq!(systemd.weights.lock().unwrap()["user-1234.slice"], 250);
        assert!(granted.raises.lock().unwrap().is_empty());
        systemd
            .weights
            .lock()
            .unwrap()
            .insert("user-1234.slice".into(), 500);
        assert!(handle(&granted, peer(1234), &raise).reply.ok);
        assert_eq!(systemd.weights.lock().unwrap()["user-1234.slice"], 500);
        assert!(granted.raises.lock().unwrap().contains_key(&1234));
    }

    /// 006 FR-027: priority over plain users is the same raise at its own level, with its own
    /// policy key and weight. Either level's weight counts as the broker's, so a caller can move
    /// between them, and each lease ends when its own key turns off.
    #[test]
    fn the_plain_level_has_its_own_key_and_weight() {
        use cedegrid::broker::request::RaiseLevel;
        use cedegrid::broker::server::expire_raises;
        let systemd = Arc::new(FakeSystemd::default());
        for kind in [UnitKind::Ordinary, UnitKind::Idle] {
            let mut show = ours(kind, 1234, "active");
            show.control_group = format!(
                "/user.slice/user-1234.slice/{}",
                cedegrid::broker::units::unit_name(kind, 1234)
            );
            systemd.put(&cedegrid::broker::units::unit_name(kind, 1234), show);
        }
        let policy = Arc::new(Mutex::new(String::new()));
        let mut broker = state(FakeSystemd::default(), &[1234], None);
        broker.systemd = Box::new(Shared(Arc::clone(&systemd)));
        let text = Arc::clone(&policy);
        broker.load_policy = Box::new(move || AdminPolicy::parse(&text.lock().unwrap(), 1000));
        let raise = |level: RaiseLevel| {
            line(&format!(
                r#"{{"protocol":1,"kind":"raise","on":true,"level":"{}"}}"#,
                level.name()
            ))
        };
        let weight = || {
            systemd
                .weights
                .lock()
                .unwrap()
                .get("user-1234.slice")
                .copied()
        };
        // Unprotected permitted does not permit the plain level.
        *policy.lock().unwrap() = "[modes]\nunprotected = true\n".into();
        let handled = handle(&broker, peer(1234), &raise(RaiseLevel::Plain));
        assert_eq!(handled.reply.code, Some(RefusalCode::RaiseRefused));
        assert_eq!(weight(), None);
        *policy.lock().unwrap() =
            "[modes]\nunprotected = true\nplain_priority = true\nplain_priority_cpu_weight = 250\n"
                .into();
        assert!(
            handle(&broker, peer(1234), &raise(RaiseLevel::Plain))
                .reply
                .ok
        );
        assert_eq!(weight(), Some(250));
        // Up to unprotected's weight and back: both are the broker's.
        assert!(
            handle(&broker, peer(1234), &raise(RaiseLevel::Unprotected))
                .reply
                .ok
        );
        assert_eq!(weight(), Some(400));
        assert!(
            handle(&broker, peer(1234), &raise(RaiseLevel::Plain))
                .reply
                .ok
        );
        assert_eq!(weight(), Some(250));
        // The plain key off ends the plain lease at the next check.
        *policy.lock().unwrap() = "[modes]\nunprotected = true\n".into();
        expire_raises(&broker, Instant::now());
        assert_eq!(weight(), None);
        assert!(broker.raises.lock().unwrap().is_empty());
    }

    /// 006 FR-016, FR-018: a raise ends at the next check, without waiting for its lease, once
    /// the administrator turns unprotected off or the member leaves the group.
    #[test]
    fn a_raise_ends_once_the_key_is_off_or_the_member_left() {
        use cedegrid::broker::server::expire_raises;
        let systemd = Arc::new(FakeSystemd::default());
        for uid in [1234, 1235] {
            for kind in [UnitKind::Ordinary, UnitKind::Idle] {
                let mut show = ours(kind, uid, "active");
                show.control_group = format!(
                    "/user.slice/user-{uid}.slice/{}",
                    cedegrid::broker::units::unit_name(kind, uid)
                );
                systemd.put(&cedegrid::broker::units::unit_name(kind, uid), show);
            }
        }
        let permitted = Arc::new(Mutex::new(true));
        let members = Arc::new(Mutex::new(BTreeSet::from([1234u32, 1235])));
        struct Members(Arc<Mutex<BTreeSet<u32>>>);
        impl Accounts for Members {
            fn primary_gid(&self, uid: u32) -> anyhow::Result<u32> {
                Ok(uid + 100)
            }
            fn is_member(&self, uid: u32) -> anyhow::Result<bool> {
                Ok(self.0.lock().unwrap().contains(&uid))
            }
        }
        let mut broker = state(FakeSystemd::default(), &[], None);
        broker.systemd = Box::new(Shared(Arc::clone(&systemd)));
        broker.accounts = Box::new(Members(Arc::clone(&members)));
        let key = Arc::clone(&permitted);
        broker.load_policy = Box::new(move || {
            AdminPolicy::parse(
                &format!("[modes]\nunprotected = {}\n", key.lock().unwrap()),
                1000,
            )
        });
        let raise = line(r#"{"protocol":1,"kind":"raise","on":true}"#);
        let now = Instant::now();
        assert!(handle(&broker, peer(1234), &raise).reply.ok);
        assert!(handle(&broker, peer(1235), &raise).reply.ok);
        expire_raises(&broker, now);
        assert_eq!(systemd.weights.lock().unwrap().len(), 2);
        // The member who left loses its raise; the other keeps its own.
        members.lock().unwrap().remove(&1235);
        expire_raises(&broker, now);
        assert_eq!(
            *systemd.weights.lock().unwrap(),
            BTreeMap::from([("user-1234.slice".to_owned(), 400)])
        );
        assert!(!broker.raises.lock().unwrap().contains_key(&1235));
        // The key off ends every raise.
        *permitted.lock().unwrap() = false;
        expire_raises(&broker, now);
        assert!(systemd.weights.lock().unwrap().is_empty());
        assert!(broker.raises.lock().unwrap().is_empty());
    }

    /// 006 FR-018: a raise ends with the broker that set it, however that broker stopped. The
    /// next broker, and the stop of this one, reset each account slice whose runtime drop-in
    /// carries the policy's weight, and nothing else.
    #[test]
    fn raises_a_stopped_broker_left_are_reset_and_nothing_else() {
        use cedegrid::broker::server::end_left_raises;
        use cedegrid::broker::systemd::raised_slices;
        let base = std::env::var_os("TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let dir = base.join(format!(".system-mode-control-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let drop_in = |name: &str, file: &str, body: &str| {
            let path = dir.join(name);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(
                path.join(file),
                format!(
                    "# This is a drop-in unit file extension, created via \"systemctl set-property\"\n# or an equivalent operation. Do not edit.\n[Slice]\n{body}\n"
                ),
            )
            .unwrap();
        };
        drop_in("user-1234.slice.d", "50-CPUWeight.conf", "CPUWeight=400");
        // An administrator's weight, one already reset, another file, a name that is not the
        // slice's, another slice, and the last line deciding either way.
        drop_in("user-1235.slice.d", "50-CPUWeight.conf", "CPUWeight=200");
        drop_in("user-1236.slice.d", "50-CPUWeight.conf", "CPUWeight=");
        drop_in(
            "user-1237.slice.d",
            "50-CPUAccounting.conf",
            "CPUWeight=400",
        );
        drop_in("user-01238.slice.d", "50-CPUWeight.conf", "CPUWeight=400");
        drop_in("system.slice.d", "50-CPUWeight.conf", "CPUWeight=400");
        drop_in(
            "user-1239.slice.d",
            "50-CPUWeight.conf",
            "CPUWeight=400\nCPUWeight=",
        );
        drop_in(
            "user-1240.slice.d",
            "50-CPUWeight.conf",
            "CPUWeight=100\nCPUWeight=400",
        );
        assert_eq!(raised_slices(&dir, 400), vec![1234, 1240]);
        assert!(raised_slices(&dir.join("missing"), 400).is_empty());
        let systemd = FakeSystemd::default();
        for (slice, weight) in [
            ("user-1234.slice", 400),
            ("user-1235.slice", 200),
            ("user-1240.slice", 400),
        ] {
            systemd.weights.lock().unwrap().insert(slice.into(), weight);
        }
        assert_eq!(end_left_raises(&systemd, 400, &dir), vec![1234, 1240]);
        assert_eq!(
            *systemd.weights.lock().unwrap(),
            BTreeMap::from([("user-1235.slice".to_owned(), 200)])
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn denied_after(systemd: &Arc<FakeSystemd>) -> BrokerState {
        let mut denied = state(FakeSystemd::default(), &[1234], None);
        denied.systemd = Box::new(Shared(Arc::clone(systemd)));
        denied
    }

    #[test]
    fn handle_checks_the_caller_before_the_request() {
        let state = state(FakeSystemd::default(), &[1234], None);
        let handled = handle(&state, peer(1235), b"garbage");
        assert_eq!(handled.reply.code, Some(RefusalCode::NotAMember));
        let handled = handle(&state, peer(1234), b"garbage");
        assert_eq!(handled.reply.code, Some(RefusalCode::Malformed));
        let handled = handle(
            &state,
            peer(1234),
            &line(r#"{"protocol":1,"kind":"status","uid":0}"#),
        );
        assert_eq!(handled.reply.code, Some(RefusalCode::IdentityInRequest));
    }

    /// FR-004 with 001 FR-059: a member in one of the policy's system ranges is a system account
    /// and is refused; the ranges are the administrator's to change.
    #[test]
    fn handle_refuses_system_accounts_in_the_policy_ranges() {
        let status = line(r#"{"protocol":1,"kind":"status"}"#);
        // 64321 is in systemd's dynamic service user range, one of the defaults.
        let defaults = state(FakeSystemd::default(), &[1234, 64321], None);
        let handled = handle(&defaults, peer(64321), &status);
        assert_eq!(handled.reply.code, Some(RefusalCode::RefusedAccount));
        let allowed = |state: &BrokerState, uid| {
            handle(state, peer(uid), &status).reply.code != Some(RefusalCode::RefusedAccount)
        };
        assert!(allowed(&defaults, 1234));
        let no_ranges = state(
            FakeSystemd::default(),
            &[1234, 64321],
            Some("[principals.system_accounts]\nuid_ranges = []\n"),
        );
        assert!(allowed(&no_ranges, 64321));
    }

    #[test]
    fn handle_applies_the_policy_to_modes() {
        let state = state(
            FakeSystemd::default(),
            &[1234],
            Some("[modes]\nallowed = [\"neutral\"]\n"),
        );
        let handled = handle(
            &state,
            peer(1234),
            &line(r#"{"protocol":1,"kind":"start","mode":"performance"}"#),
        );
        assert_eq!(handled.reply.code, Some(RefusalCode::ModeNotAllowed));
        assert!(handled.reply.message.unwrap().contains("modes.allowed"));
        let handled = handle(
            &state,
            peer(1234),
            &line(r#"{"protocol":1,"kind":"start","mode":"neutral"}"#),
        );
        assert!(handled.reply.ok);
        assert_eq!(handled.kind, "start");
        assert_eq!(handled.reply.units.unwrap().len(), 2);
    }

    #[test]
    fn auto_places_the_ordinary_unit_by_the_host_layout() {
        for (splits, slice) in [(true, "user-1234.slice"), (false, "cedegrid.slice")] {
            let systemd = Arc::new(FakeSystemd::default());
            let mut state = state(FakeSystemd::default(), &[1234], None);
            state.systemd = Box::new(Shared(Arc::clone(&systemd)));
            state.user_slice_splits_cpu = Box::new(move || splits);
            let handled = handle(
                &state,
                peer(1234),
                &line(r#"{"protocol":1,"kind":"start"}"#),
            );
            assert!(handled.reply.ok, "{handled:?}");
            let calls = systemd.calls();
            assert!(
                calls.contains(&format!("start cedegrid-u1234.service {slice}")),
                "{calls:?}"
            );
            assert!(
                calls.contains(&"start cedegrid-idle-u1234.service cedegrid_idle.slice".to_owned())
            );
        }
    }

    #[test]
    fn start_sets_cpu_idle_or_stops_the_idle_unit() {
        let systemd = Arc::new(FakeSystemd::default());
        let mut state = state(FakeSystemd::default(), &[1234], None);
        state.systemd = Box::new(Shared(Arc::clone(&systemd)));
        let handled = handle(
            &state,
            peer(1234),
            &line(r#"{"protocol":1,"kind":"start"}"#),
        );
        assert!(handled.reply.ok, "{handled:?}");
        assert_eq!(
            state
                .cgroups
                .read("/cedegrid_idle.slice", "cpu.idle")
                .unwrap(),
            "1"
        );
        let systemd = Arc::new(FakeSystemd::default());
        state.systemd = Box::new(Shared(Arc::clone(&systemd)));
        state.cgroups = Box::new(MemCgroups {
            read_only: true,
            ..MemCgroups::default()
        });
        let handled = handle(
            &state,
            peer(1234),
            &line(r#"{"protocol":1,"kind":"start"}"#),
        );
        assert_eq!(handled.reply.code, Some(RefusalCode::SystemdFailed));
        assert!(
            systemd
                .calls()
                .contains(&"stop cedegrid-idle-u1234.service".to_owned())
        );
    }

    #[test]
    fn a_refused_policy_file_stops_every_request() {
        let state = state(FakeSystemd::default(), &[1234], Some("unknown = 1\n"));
        let handled = handle(
            &state,
            peer(1234),
            &line(r#"{"protocol":1,"kind":"status"}"#),
        );
        assert_eq!(handled.reply.code, Some(RefusalCode::PolicyRefused));
    }

    #[test]
    fn a_move_needs_both_units_running() {
        let state = state(FakeSystemd::default(), &[1234], None);
        let handled = handle(
            &state,
            peer(1234),
            &line(r#"{"protocol":1,"kind":"move","job":"j","mode":"neutral"}"#),
        );
        assert_eq!(handled.reply.code, Some(RefusalCode::MoveRefused));
        assert!(
            handled
                .reply
                .message
                .unwrap()
                .contains("cedegrid system start")
        );
    }

    #[test]
    fn systemd_failures_are_reported() {
        let systemd = FakeSystemd {
            fail_start: true,
            ..FakeSystemd::default()
        };
        let state = state(systemd, &[1234], None);
        let handled = handle(
            &state,
            peer(1234),
            &line(r#"{"protocol":1,"kind":"start"}"#),
        );
        assert_eq!(handled.reply.code, Some(RefusalCode::SystemdFailed));
    }

    #[test]
    fn rate_limit_is_per_user_and_refills() {
        let mut rate = RateLimiter::new(3, 2);
        let now = Instant::now();
        assert!((0..3).all(|_| rate.allow(1000, now)));
        assert!(!rate.allow(1000, now));
        // Another user is not held back by the first.
        assert!(rate.allow(1001, now));
        assert!(rate.allow(1000, now + Duration::from_millis(500)));
        assert!(!rate.allow(1000, now + Duration::from_millis(500)));
        assert!(rate.allow(1000, now + Duration::from_secs(10)));
    }

    fn pair() -> (UnixStream, UnixStream) {
        UnixStream::pair().unwrap()
    }

    #[test]
    fn read_frame_stops_at_the_newline_the_limit_or_the_deadline() {
        let (mut client, server) = pair();
        client.write_all(b"{\"protocol\":1}\nextra").unwrap();
        let frame = read_frame(&server, Instant::now() + Duration::from_secs(1)).unwrap();
        assert!(frame.starts_with(b"{\"protocol\":1}\n"));

        let (mut client, server) = pair();
        let writer = std::thread::spawn(move || {
            let _ = client.write_all(&vec![b' '; MAX_REQUEST_BYTES + 10_000]);
            client
        });
        let frame = read_frame(&server, Instant::now() + Duration::from_secs(2)).unwrap();
        assert_eq!(frame.len(), MAX_REQUEST_BYTES + 1);
        drop(server);
        drop(writer.join().unwrap());

        let (mut client, server) = pair();
        client.write_all(b"{\"protocol\":").unwrap();
        let started = Instant::now();
        let refusal = read_frame(&server, started + Duration::from_millis(200)).unwrap_err();
        assert_eq!(refusal.code, RefusalCode::RequestTimeout);
        assert!(started.elapsed() < Duration::from_millis(600));
    }

    #[test]
    fn a_connection_gets_one_reply_line() {
        let state = state(FakeSystemd::default(), &[1234], None);
        let (mut client, server) = pair();
        client
            .write_all(b"{\"protocol\":1,\"kind\":\"status\"}\n")
            .unwrap();
        serve_connection(&state, &server, peer(1234), Instant::now());
        drop(server);
        let mut reply = String::new();
        BufReader::new(&client).read_line(&mut reply).unwrap();
        let reply: Reply = serde_json::from_str(&reply).unwrap();
        assert!(reply.ok);
        let units: Vec<UnitStatus> = reply.units.unwrap();
        assert!(units.iter().all(|unit| unit.state == "absent"));
    }

    #[test]
    fn the_listener_serves_real_connections_with_real_peer_credentials() {
        // SAFETY: getuid has no preconditions and cannot fail.
        let me = unsafe { libc::getuid() };
        let base = std::env::var_os("TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let dir = base.join(format!(".sm-listen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("b.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let mut state = state(FakeSystemd::default(), &[me], None);
        state.host_uid_min = 1;
        let state = Arc::new(state);
        std::thread::spawn(move || serve(listener, state));
        let ask = |body: &str| -> Reply {
            let mut stream = UnixStream::connect(&path).unwrap();
            stream.write_all(body.as_bytes()).unwrap();
            let mut reply = String::new();
            BufReader::new(&stream).read_line(&mut reply).unwrap();
            serde_json::from_str(&reply).unwrap()
        };
        let reply = ask("{\"protocol\":1,\"kind\":\"start\",\"mode\":\"yield_first\"}\n");
        assert!(reply.ok, "{reply:?}");
        let units = reply.units.unwrap();
        assert_eq!(units[0].name, format!("cedegrid-u{me}.service"));
        let reply = ask("{\"protocol\":1,\"kind\":\"start\",\"mode\":\"aggressive\"}\n");
        assert_eq!(reply.code, Some(RefusalCode::ModeNotAllowed));
        // The burst is spent: further requests are rate limited, not queued forever.
        let limited = (0..12)
            .map(|_| ask("{\"protocol\":1,\"kind\":\"status\"}\n"))
            .filter(|reply| reply.code == Some(RefusalCode::RateLimited))
            .count();
        assert!(limited > 0);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = ModeName::Neutral;
    }
}

#[test]
fn pam_session_comes_from_the_policy() {
    use cedegrid::broker::units::PamSession;
    assert_eq!(
        AdminPolicy::defaults(1000).unwrap().pam_session.value,
        PamSession::None
    );
    for (name, expected) in [
        ("none", PamSession::None),
        ("cedegrid", PamSession::CedegridLimits),
        ("login", PamSession::HostLogin),
    ] {
        let policy =
            AdminPolicy::parse(&format!("[units]\npam_session = \"{name}\"\n"), 1000).unwrap();
        assert_eq!(policy.pam_session.value, expected);
        assert_eq!(policy.pam_session.source, Source::File);
    }
    assert!(AdminPolicy::parse("[units]\npam_session = \"system-auth\"\n", 1000).is_err());
}

#[test]
fn ordinary_slice_choice_follows_n4() {
    use cedegrid::broker::units::{OrdinarySlice, SliceChoice};
    let policy = AdminPolicy::defaults(1000).unwrap();
    assert_eq!(policy.ordinary_slice.value, SliceChoice::Auto);
    assert_eq!(SliceChoice::Auto.resolve(true), OrdinarySlice::UserSlice);
    assert_eq!(
        SliceChoice::Auto.resolve(false),
        OrdinarySlice::CedegridSlice
    );
    assert_eq!(SliceChoice::User.resolve(false), OrdinarySlice::UserSlice);
    assert_eq!(
        SliceChoice::Cedegrid.resolve(true),
        OrdinarySlice::CedegridSlice
    );
    for (name, expected) in [
        ("auto", SliceChoice::Auto),
        ("user", SliceChoice::User),
        ("cedegrid", SliceChoice::Cedegrid),
    ] {
        let policy =
            AdminPolicy::parse(&format!("[units]\nordinary_slice = \"{name}\"\n"), 1000).unwrap();
        assert_eq!(policy.ordinary_slice.value, expected);
    }
    assert!(AdminPolicy::parse("[units]\nordinary_slice = \"root\"\n", 1000).is_err());
}

/// Without the system package, `cedegrid system` changes nothing and prints no notice (FR-013).
#[test]
fn system_command_without_the_package() {
    if std::path::Path::new(cedegrid::broker::SOCKET_PATH).exists() {
        return;
    }
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_cedegrid"))
        .args(["system", "status"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        r#"{"system":false,"units":[]}"#
    );
    assert!(output.stderr.is_empty(), "{output:?}");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_cedegrid"))
        .args(["system", "start", "--require-system"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("not installed"));
}

// T043, T047: the OOM pass's pure steps.

mod oom_pass {
    use cedegrid::broker::ModeName;
    use cedegrid::broker::oom::{
        PRINCIPAL_JOIN, PRINCIPAL_LEAVE, PassView, Principals, ProcessSample, UnitProcess,
        UserMemoryMap, another_over_share, fair_shares, group_tasks, leaf_in_unit, memory_by_user,
        task_mode, user_writes,
    };
    use cedegrid::broker::score::{Lowering, OverShareMarking, ProcessKey};
    use cedegrid::broker::units::UnitKind;
    use cedegrid::config::SystemAccounts;
    use std::collections::{BTreeMap, BTreeSet};
    use std::time::{Duration, Instant};

    /// Users as the default policy tells them: at or above 1000 and outside systemd's
    /// non-human ranges (001 FR-059).
    fn users(uid: u32) -> bool {
        uid >= 1000 && !SystemAccounts::default().contains(uid)
    }

    fn sample(uid: Option<u32>, pages: Option<u64>) -> ProcessSample {
        ProcessSample { uid, pages }
    }

    #[test]
    fn memory_is_summed_per_user_and_unknown_spreads() {
        let map = memory_by_user(&[
            sample(Some(1000), Some(10)),
            sample(Some(1000), Some(5)),
            sample(Some(1001), Some(7)),
            sample(Some(1001), None),
            sample(None, Some(3)),
        ]);
        assert_eq!(map.by_user[&1000], Some(15));
        assert_eq!(map.by_user[&1001], None);
        assert_eq!(map.unattributed, Some(3));
        let map = memory_by_user(&[sample(None, None)]);
        assert_eq!(map.unattributed, None);
    }

    fn memory(entries: &[(u32, Option<u64>)]) -> UserMemoryMap {
        UserMemoryMap {
            by_user: entries.iter().copied().collect(),
            unattributed: Some(0),
        }
    }

    #[test]
    fn principals_join_after_the_window_and_leave_after_a_minute() {
        let mut principals = Principals::default();
        let start = Instant::now();
        // 999 and 64321, a dynamic service user in systemd's range, are system accounts, never
        // principals however much they hold (001 FR-059).
        let busy = memory(&[
            (1000, Some(50)),
            (999, Some(500)),
            (64321, Some(500)),
            (1001, Some(0)),
        ]);
        // 1% of 1000 pages is 10 pages.
        principals.update(start, &busy, 1000, &users, 1);
        assert!(principals.members().is_empty());
        principals.update(start + PRINCIPAL_JOIN, &busy, 1000, &users, 1);
        assert_eq!(principals.members(), &BTreeSet::from([1000]));
        // Below the threshold: stays until the leave window ends.
        let idle = memory(&[(1000, Some(1)), (1001, Some(0))]);
        let t = start + PRINCIPAL_JOIN + Duration::from_secs(1);
        principals.update(t, &idle, 1000, &users, 1);
        principals.update(
            t + PRINCIPAL_LEAVE - Duration::from_secs(1),
            &idle,
            1000,
            &users,
            1,
        );
        assert!(principals.members().contains(&1000));
        principals.update(t + PRINCIPAL_LEAVE, &idle, 1000, &users, 1);
        assert!(principals.members().is_empty());
        // Unknown memory counts as at the threshold.
        let unknown = memory(&[(1002, None)]);
        principals.update(t, &unknown, 1000, &users, 1);
        principals.update(t + PRINCIPAL_JOIN, &unknown, 1000, &users, 1);
        assert!(principals.members().contains(&1002));
        // A user whose processes are all gone leaves at once.
        principals.update(t + PRINCIPAL_JOIN, &memory(&[]), 1000, &users, 1);
        assert!(principals.members().is_empty());
    }

    #[test]
    fn fair_shares_are_max_min_of_what_others_leave() {
        let mem = memory(&[
            (1000, Some(600)),
            (1001, Some(600)),
            (1002, Some(100)),
            (500, Some(100)),
        ]);
        let principals = BTreeSet::from([1000, 1001, 1002]);
        let shares = fair_shares(1000, &mem, &principals);
        // 900 pages after user 500's 100: 1002 keeps its 100, the rest split 400 and 400.
        assert_eq!(shares[&1002], 100);
        assert_eq!(shares[&1000], 400);
        assert_eq!(shares[&1001], 400);
        assert!(another_over_share(1000, &mem, &shares));
        assert!(another_over_share(1002, &mem, &shares));
        // Room for everyone: nobody is over.
        let roomy = fair_shares(10_000, &mem, &principals);
        assert!(!another_over_share(1000, &mem, &roomy));
        // An unknown principal never enlarges another's share and is never "over".
        let unknown = memory(&[(1000, Some(600)), (1001, None)]);
        let shares = fair_shares(1000, &unknown, &BTreeSet::from([1000, 1001]));
        assert_eq!(shares[&1000], 500);
        assert!(!another_over_share(1000, &unknown, &shares));
    }

    #[test]
    fn leaves_are_found_only_inside_the_unit() {
        let unit = "/user.slice/user-1000.slice/cedegrid-u1000.service";
        assert_eq!(leaf_in_unit(unit, unit), Some(None));
        assert_eq!(
            leaf_in_unit(unit, &format!("{unit}/cedegrid-neutral-ab/sub")),
            Some(Some("cedegrid-neutral-ab".to_owned()))
        );
        assert_eq!(leaf_in_unit(unit, &format!("{unit}x/leaf")), None);
        assert_eq!(
            leaf_in_unit(unit, "/user.slice/user-1000.slice/session-3.scope"),
            None
        );
        assert_eq!(leaf_in_unit(unit, &format!("{unit}/")), None);
        assert_eq!(
            leaf_in_unit(unit, &format!("{unit}/jobs/cedegrid-neutral-a.00ff")),
            Some(Some("cedegrid-neutral-a.00ff".to_owned()))
        );
        assert_eq!(
            leaf_in_unit(unit, &format!("{unit}/agent")),
            Some(Some("agent".to_owned()))
        );
    }

    #[test]
    fn task_modes_come_from_the_unit_and_leaf_names() {
        assert_eq!(
            task_mode(UnitKind::Idle, Some("cedegrid-performance-1")),
            ModeName::YieldFirst
        );
        assert_eq!(
            task_mode(UnitKind::Ordinary, Some("cedegrid-performance-0a1b")),
            ModeName::Performance
        );
        assert_eq!(
            task_mode(UnitKind::Ordinary, Some("cedegrid-yield_first-0a1b")),
            ModeName::YieldFirst
        );
        for leaf in [
            Some("cedegrid-0a1b"),
            Some("other"),
            Some("cedegrid-turbo-1"),
            None,
        ] {
            assert_eq!(
                task_mode(UnitKind::Ordinary, leaf),
                ModeName::Neutral,
                "{leaf:?}"
            );
        }
    }

    fn unit_process(kind: UnitKind, leaf: &str, pid: u32, start: u64, pages: u64) -> UnitProcess {
        UnitProcess {
            uid: 1000,
            kind,
            leaf: Some(leaf.to_owned()),
            key: ProcessKey {
                pid,
                start_time: start,
            },
            pages,
            current: 0,
        }
    }

    #[test]
    fn tasks_are_grouped_by_leaf_and_start_at_their_oldest_process() {
        let tasks = group_tasks(&[
            unit_process(UnitKind::Ordinary, "cedegrid-neutral-a", 1, 50, 10),
            unit_process(UnitKind::Ordinary, "cedegrid-neutral-a", 2, 40, 10),
            unit_process(UnitKind::Idle, "cedegrid-neutral-a", 3, 10, 10),
        ]);
        assert_eq!(tasks.len(), 2);
        let ordinary = tasks.iter().find(|task| task.processes.len() == 2).unwrap();
        assert_eq!(ordinary.started, 40);
        assert_eq!(ordinary.mode, ModeName::Neutral);
        let idle = tasks.iter().find(|task| task.processes.len() == 1).unwrap();
        assert_eq!(idle.mode, ModeName::YieldFirst);
    }

    #[test]
    fn a_pass_lowers_in_share_work_while_another_user_is_over() {
        let mem = memory(&[(1000, Some(300)), (1001, Some(900))]);
        let principals = BTreeSet::from([1000, 1001]);
        let shares = fair_shares(1000, &mem, &principals);
        assert_eq!(shares[&1000], 300);
        let view = PassView {
            total_pages: 2000,
            memory: &mem,
            shares: &shares,
            lowering: Lowering::WhileAnotherOverShare,
            marking: OverShareMarking::YieldFirstAndNeutral,
        };
        let processes = [unit_process(
            UnitKind::Ordinary,
            "cedegrid-performance-a",
            1,
            10,
            300,
        )];
        let writes = user_writes(&view, 1000, &processes, None);
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].value, -150);
        // Memory unknown for longer than the window: everything returns to 0.
        let mut lowered = processes.clone();
        lowered[0].current = -150;
        let writes = user_writes(&view, 1000, &lowered, Some(Duration::from_secs(11)));
        assert_eq!(writes[0].value, 0);
        // Not a principal: nothing is lowered or marked.
        let writes = user_writes(&view, 1002, &processes, None);
        assert!(writes.is_empty());
    }

    #[test]
    fn a_pass_marks_over_share_neutral_work_but_not_performance() {
        let mem = memory(&[(1000, Some(900)), (1001, Some(500))]);
        let principals = BTreeSet::from([1000, 1001]);
        let shares: BTreeMap<u32, u64> = fair_shares(1000, &mem, &principals);
        assert_eq!(shares[&1000], 500);
        let view = PassView {
            total_pages: 2000,
            memory: &mem,
            shares: &shares,
            lowering: Lowering::WhileAnotherOverShare,
            marking: OverShareMarking::YieldFirstAndNeutral,
        };
        let processes = [
            unit_process(UnitKind::Ordinary, "cedegrid-neutral-a", 1, 10, 500),
            unit_process(UnitKind::Ordinary, "cedegrid-neutral-b", 2, 20, 200),
            unit_process(UnitKind::Ordinary, "cedegrid-performance-c", 3, 30, 200),
        ];
        let mut writes: Vec<(u32, i32)> = user_writes(&view, 1000, &processes, None)
            .into_iter()
            .map(|write| (write.key.pid, write.value))
            .collect();
        writes.sort();
        // User 1001 is not over its share, so nothing of 1000's is lowered under N1 B; the
        // Neutral task past the share is marked and the Performance one is not.
        assert_eq!(writes, vec![(2, 1000)]);
    }
}

// T047, T048: the OOM loop's host readers and pass, against a fake /proc, cgroup tree and
// process access.

mod oom_loop {
    use cedegrid::broker::oom::{
        FoundUnit, HeldProcess, HostReadings, OomLoop, PRINCIPAL_JOIN, ProcessAccess,
        ProcessSample, ScannedProcess, ScoredUnit, find_units, parse_meminfo, parse_status_memory,
        read_host, unit_pids,
    };
    use cedegrid::broker::process_handle::{
        ProcessIds, parse_cgroup_v2, parse_ids, parse_start_time,
    };
    use cedegrid::broker::score::{Lowering, OverShareMarking};
    use cedegrid::broker::units::UnitKind;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::time::Instant;

    fn users(uid: u32) -> bool {
        uid >= 1000
    }

    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Self {
            let base = std::env::var_os("TMPDIR")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir);
            let path = base.join(format!(".sm-oom-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Dir(path)
        }

        fn write(&self, relative: &str, text: &str) {
            let path = self.0.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn stat(pid: u32, start: u64) -> String {
        format!("{pid} (x y) S 1 1 1 0 -1 0 0 0 0 0 1 1 0 0 20 0 1 0 {start} 1000 10\n")
    }

    #[test]
    fn proc_text_parsers() {
        assert_eq!(parse_start_time(&stat(5, 777)), Some(777));
        assert_eq!(
            parse_ids("Uid:\t1\t1\t1\t1\nGid:\t2\t2\t2\t2\n").map(|ids| ids.all_are(1, 2)),
            Some(true)
        );
        assert_eq!(parse_cgroup_v2("0::/a/b\n"), Some("/a/b".to_owned()));
        assert_eq!(
            parse_meminfo("MemTotal:  1000 kB\nMemFree: 1 kB\nSwapTotal:  24 kB\n"),
            Some((1000, 24))
        );
        assert_eq!(parse_meminfo("MemFree: 1 kB\n"), None);
        assert_eq!(
            parse_status_memory("Uid:\t1000\t1000\t1000\t1000\nVmRSS:\t  400 kB\nVmSwap:\t 8 kB\n"),
            (Some(1000), Some(408))
        );
        // A kernel thread has no VmRSS and no memory of its own.
        assert_eq!(
            parse_status_memory("Uid:\t0\t0\t0\t0\n"),
            (Some(0), Some(0))
        );
        assert_eq!(parse_status_memory("VmRSS:\t x kB\n"), (None, None));
    }

    #[test]
    fn plentiful_memory_is_a_quarter_free_and_little_swap() {
        use cedegrid::broker::oom::memory_is_plentiful;
        let meminfo = |available: u64, swap_free: u64| {
            format!(
                "MemTotal: 1000 kB\nMemAvailable: {available} kB\nSwapTotal: 100 kB\nSwapFree: {swap_free} kB\n"
            )
        };
        assert!(memory_is_plentiful(&meminfo(250, 100)));
        assert!(memory_is_plentiful(&meminfo(250, 50)));
        assert!(!memory_is_plentiful(&meminfo(249, 100)));
        assert!(!memory_is_plentiful(&meminfo(900, 49)));
        assert!(!memory_is_plentiful("MemTotal: 1000 kB\n"));
        assert!(!memory_is_plentiful(""));
    }

    #[test]
    fn the_host_scan_reads_owners_memory_and_start_times() {
        let proc = Dir::new("proc");
        proc.write("meminfo", "MemTotal: 4000 kB\nSwapTotal: 400 kB\n");
        proc.write("10/stat", &stat(10, 100));
        proc.write(
            "10/status",
            "Uid:\t1000\t1000\t1000\t1000\nVmRSS:\t 40 kB\nVmSwap:\t 4 kB\n",
        );
        proc.write("11/stat", &stat(11, 101));
        proc.write("11/status", "Uid:\tbad\nVmRSS:\t 8 kB\n");
        proc.write("12/status", "Uid:\t1\t1\t1\t1\n");
        proc.write("self/stat", &stat(1, 1));
        let host = read_host(&proc.0, 4).unwrap();
        assert_eq!(host.total_pages, 1100);
        assert_eq!(host.capacity_pages, 1000);
        assert_eq!(host.processes.len(), 2);
        assert_eq!(host.processes[&10].start_time, 100);
        assert_eq!(
            host.processes[&10].sample,
            ProcessSample {
                uid: Some(1000),
                pages: Some(11)
            }
        );
        assert_eq!(host.processes[&11].sample.uid, None);
    }

    #[test]
    fn units_are_found_by_name_and_slice() {
        let cg = Dir::new("cgroup");
        for dir in [
            "cedegrid_idle.slice/cedegrid-idle-u1000.service",
            "cedegrid_idle.slice/other.service",
            "cedegrid.slice/cedegrid-u1001.service",
            "user.slice/user-1000.slice/cedegrid-u1000.service",
            "user.slice/user-1000.slice/cedegrid-u1002.service",
            "user.slice/user-1000.slice/session-1.scope",
        ] {
            std::fs::create_dir_all(cg.0.join(dir)).unwrap();
        }
        let mut units = find_units(&cg.0);
        units.sort_by(|a, b| a.cgroup.cmp(&b.cgroup));
        assert!(units.iter().all(|unit| unit.id != 0));
        let found: Vec<(UnitKind, u32, &str)> = units
            .iter()
            .map(|unit| (unit.kind, unit.uid, unit.cgroup.as_str()))
            .collect();
        assert_eq!(
            found,
            vec![
                (
                    UnitKind::Ordinary,
                    1001,
                    "/cedegrid.slice/cedegrid-u1001.service"
                ),
                (
                    UnitKind::Idle,
                    1000,
                    "/cedegrid_idle.slice/cedegrid-idle-u1000.service"
                ),
                (
                    UnitKind::Ordinary,
                    1000,
                    "/user.slice/user-1000.slice/cedegrid-u1000.service"
                ),
            ]
        );
        cg.write("cedegrid.slice/cedegrid-u1001.service/cgroup.procs", "7\n");
        cg.write(
            "cedegrid.slice/cedegrid-u1001.service/agent/cgroup.procs",
            "8\n",
        );
        cg.write(
            "cedegrid.slice/cedegrid-u1001.service/cedegrid-neutral-a/cgroup.procs",
            "9\n10\n",
        );
        assert_eq!(
            unit_pids(&cg.0, "/cedegrid.slice/cedegrid-u1001.service"),
            vec![7, 8, 9, 10]
        );
    }

    struct FakeProcess {
        start_time: u64,
        cgroup: String,
        uid: u32,
        gid: u32,
        score: Mutex<i32>,
    }

    struct Fake(BTreeMap<u32, std::sync::Arc<FakeProcess>>);

    struct Held(std::sync::Arc<FakeProcess>);

    impl HeldProcess for Held {
        fn start_time(&self) -> u64 {
            self.0.start_time
        }
        fn cgroup(&self) -> anyhow::Result<String> {
            Ok(self.0.cgroup.clone())
        }
        fn ids(&self) -> anyhow::Result<ProcessIds> {
            Ok(ProcessIds {
                uids: [self.0.uid; 4],
                gids: [self.0.gid; 4],
            })
        }
        fn oom_score_adj(&self) -> anyhow::Result<i32> {
            Ok(*self.0.score.lock().unwrap())
        }
        fn set_oom_score_adj(&self, value: i32) -> anyhow::Result<()> {
            *self.0.score.lock().unwrap() = value;
            Ok(())
        }
    }

    impl ProcessAccess for Fake {
        fn open(&self, pid: u32, start_time: Option<u64>) -> anyhow::Result<Box<dyn HeldProcess>> {
            let process = self.0.get(&pid).ok_or_else(|| anyhow::anyhow!("gone"))?;
            anyhow::ensure!(
                start_time.is_none_or(|start| process.start_time == start),
                "reused"
            );
            Ok(Box::new(Held(std::sync::Arc::clone(process))))
        }
    }

    fn fake(pid: u32, start: u64, cgroup: &str, uid: u32) -> (u32, std::sync::Arc<FakeProcess>) {
        (
            pid,
            std::sync::Arc::new(FakeProcess {
                start_time: start,
                cgroup: cgroup.into(),
                uid,
                gid: uid + 100,
                score: Mutex::new(0),
            }),
        )
    }

    const UNIT: &str = "/cedegrid.slice/cedegrid-u1000.service";

    fn scan(pid: u32, start: u64, uid: u32, pages: u64) -> (u32, ScannedProcess) {
        (
            pid,
            ScannedProcess {
                start_time: start,
                sample: ProcessSample {
                    uid: Some(uid),
                    pages: Some(pages),
                },
            },
        )
    }

    #[test]
    fn a_pass_writes_only_verified_unit_processes() {
        let access = Fake(BTreeMap::from([
            fake(1, 10, &format!("{UNIT}/cedegrid-neutral-a"), 1000),
            // Same user, but outside the unit.
            fake(2, 20, "/user.slice/user-1000.slice/session-1.scope", 1000),
            // Inside the unit but another user's IDs (a setuid program, say).
            fake(3, 30, &format!("{UNIT}/cedegrid-neutral-a"), 0),
            // Its PID was reused since the scan.
            fake(4, 99, &format!("{UNIT}/cedegrid-neutral-a"), 1000),
        ]));
        let host = HostReadings {
            total_pages: 2000,
            capacity_pages: 1000,
            processes: BTreeMap::from([
                scan(1, 10, 1000, 300),
                scan(2, 20, 1000, 50),
                scan(3, 30, 1000, 10),
                scan(4, 40, 1000, 10),
                scan(9, 90, 1001, 900),
            ]),
        };
        let units = [ScoredUnit {
            unit: FoundUnit {
                kind: UnitKind::Ordinary,
                uid: 1000,
                cgroup: UNIT.into(),
                id: 0,
            },
            gid: 1100,
        }];
        let pids = |_: &ScoredUnit| vec![1, 2, 3, 4];
        let mut oom = OomLoop::default();
        let start = Instant::now();
        let run = |oom: &mut OomLoop, now| {
            oom.pass(
                now,
                &host,
                &units,
                &pids,
                &access,
                &users,
                &|uid| uid,
                1,
                Lowering::WhileAnotherOverShare,
                OverShareMarking::YieldFirstAndNeutral,
            )
        };
        // Principals have not joined yet: nobody has a share, nothing is lowered.
        assert!(run(&mut oom, start).is_empty());
        let writes = run(&mut oom, start + PRINCIPAL_JOIN);
        let written: Vec<(u32, i32)> = writes
            .iter()
            .map(|write| (write.key.pid, write.new))
            .collect();
        // User 1001 holds 900 of 1000 pages; user 1000's 300 are within its share, and only the
        // verified process 1 is lowered.
        assert_eq!(written, vec![(1, -150)]);
        assert!(writes.iter().all(|write| write.result.is_ok()));
        assert_eq!(access.0[&1].score.lock().unwrap().clone(), -150);
        for pid in [2, 3, 4] {
            assert_eq!(access.0[&pid].score.lock().unwrap().clone(), 0, "{pid}");
        }
        // Nothing changed: no write.
        assert!(run(&mut oom, start + PRINCIPAL_JOIN * 2).is_empty());
    }

    /// 001 FR-059: a rootless container's process under a subordinate user ID counts toward the
    /// range's owner, one person with one share, and is never written.
    #[test]
    fn a_subordinate_user_id_counts_toward_its_owner() {
        let access = Fake(BTreeMap::from([fake(
            1,
            10,
            &format!("{UNIT}/cedegrid-neutral-a"),
            1000,
        )]));
        // User 1001 holds 400 pages itself and 500 through subordinate user ID 100998.
        let host = HostReadings {
            total_pages: 2000,
            capacity_pages: 1000,
            processes: BTreeMap::from([
                scan(1, 10, 1000, 300),
                scan(8, 80, 1001, 400),
                scan(9, 90, 100_998, 500),
            ]),
        };
        let units = [ScoredUnit {
            unit: FoundUnit {
                kind: UnitKind::Ordinary,
                uid: 1000,
                cgroup: UNIT.into(),
                id: 0,
            },
            gid: 1100,
        }];
        let owner = |uid: u32| if uid == 100_998 { 1001 } else { uid };
        let mut oom = OomLoop::default();
        let start = Instant::now();
        let mut written = Vec::new();
        for now in [start, start + PRINCIPAL_JOIN] {
            written = oom
                .pass(
                    now,
                    &host,
                    &units,
                    &|_: &ScoredUnit| vec![1],
                    &access,
                    &users,
                    &owner,
                    1,
                    Lowering::WhileAnotherOverShare,
                    OverShareMarking::YieldFirstAndNeutral,
                )
                .iter()
                .map(|write| (write.key.pid, write.new))
                .collect::<Vec<_>>();
        }
        // As if user 1001 held all 900 pages: user 1000's 300 are within its share and lowered.
        assert_eq!(written, vec![(1, -150)]);
    }

    #[test]
    fn a_quiet_host_reads_no_unit_process() {
        struct Refuse;
        impl ProcessAccess for Refuse {
            fn open(&self, _: u32, _: Option<u64>) -> anyhow::Result<Box<dyn HeldProcess>> {
                panic!("no process should be opened while nobody is over its share")
            }
        }
        let host = HostReadings {
            total_pages: 2000,
            capacity_pages: 1000,
            processes: BTreeMap::from([scan(1, 10, 1000, 300), scan(9, 90, 1001, 300)]),
        };
        let units = [ScoredUnit {
            unit: FoundUnit {
                kind: UnitKind::Ordinary,
                uid: 1000,
                cgroup: UNIT.into(),
                id: 0,
            },
            gid: 1100,
        }];
        let mut oom = OomLoop::default();
        let start = Instant::now();
        for step in 0..3 {
            let writes = oom.pass(
                start + PRINCIPAL_JOIN * step,
                &host,
                &units,
                &|_: &ScoredUnit| vec![1],
                &Refuse,
                &users,
                &|uid| uid,
                1,
                Lowering::WhileAnotherOverShare,
                OverShareMarking::YieldFirstAndNeutral,
            );
            assert!(writes.is_empty());
        }
    }

    #[test]
    fn unreadable_memory_returns_scores_after_the_window() {
        let access = Fake(BTreeMap::from([fake(
            1,
            10,
            &format!("{UNIT}/cedegrid-neutral-a"),
            1000,
        )]));
        *access.0[&1].score.lock().unwrap() = -150;
        let mut host = HostReadings {
            total_pages: 2000,
            capacity_pages: 1000,
            processes: BTreeMap::from([scan(1, 10, 1000, 300), scan(9, 90, 1001, 900)]),
        };
        host.processes.get_mut(&1).unwrap().sample.pages = None;
        let units = [ScoredUnit {
            unit: FoundUnit {
                kind: UnitKind::Ordinary,
                uid: 1000,
                cgroup: UNIT.into(),
                id: 0,
            },
            gid: 1100,
        }];
        let pids = |_: &ScoredUnit| vec![1];
        let mut oom = OomLoop::default();
        let start = Instant::now();
        let run = |oom: &mut OomLoop, now| {
            oom.pass(
                now,
                &host,
                &units,
                &pids,
                &access,
                &users,
                &|uid| uid,
                1,
                Lowering::WhileAnotherOverShare,
                OverShareMarking::YieldFirstAndNeutral,
            )
        };
        assert!(run(&mut oom, start).is_empty());
        assert!(run(&mut oom, start + std::time::Duration::from_secs(5)).is_empty());
        let writes = run(&mut oom, start + std::time::Duration::from_secs(10));
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].new, 0);
        let _ = Path::new("/");
    }
}

// T036, T039: moving a job between the caller's two units, against a fake cgroup tree.

mod job_moves {
    use cedegrid::broker::ModeName;
    use cedegrid::broker::moves::{CallerUnits, CgroupFs, move_job};
    use cedegrid::broker::oom::{HeldProcess, ProcessAccess};
    use cedegrid::broker::process_handle::ProcessIds;
    use cedegrid::broker::request::{JobId, RefusalCode};
    use cedegrid::broker::units::Caller;
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Node {
        procs: Vec<u32>,
        frozen: bool,
        children: BTreeSet<String>,
    }

    struct Proc {
        cgroup: String,
        uid: u32,
        gid: u32,
    }

    #[derive(Default)]
    struct World {
        nodes: BTreeMap<String, Node>,
        procs: BTreeMap<u32, Proc>,
        never_freezes: bool,
        refuse_pid: Option<u32>,
    }

    #[derive(Clone, Default)]
    struct Fake(Arc<Mutex<World>>);

    impl Fake {
        fn cgroup(&self, path: &str, procs: &[u32]) {
            let mut world = self.0.lock().unwrap();
            if let Some((parent, name)) = path.rsplit_once('/') {
                world
                    .nodes
                    .entry(parent.to_owned())
                    .or_default()
                    .children
                    .insert(name.to_owned());
            }
            world.nodes.entry(path.to_owned()).or_default().procs = procs.to_vec();
            for &pid in procs {
                world.procs.insert(
                    pid,
                    Proc {
                        cgroup: path.to_owned(),
                        uid: 1000,
                        gid: 1100,
                    },
                );
            }
        }

        fn procs(&self, path: &str) -> Vec<u32> {
            self.0.lock().unwrap().nodes[path].procs.clone()
        }

        fn frozen(&self, path: &str) -> bool {
            self.0.lock().unwrap().nodes[path].frozen
        }
    }

    impl CgroupFs for Fake {
        fn read(&self, cgroup: &str, file: &str) -> anyhow::Result<String> {
            let world = self.0.lock().unwrap();
            let node = world
                .nodes
                .get(cgroup)
                .ok_or_else(|| anyhow::anyhow!("no cgroup"))?;
            Ok(match file {
                "cgroup.procs" => node.procs.iter().map(|pid| format!("{pid}\n")).collect(),
                "cgroup.events" => format!(
                    "populated 1\nfrozen {}\n",
                    u8::from(node.frozen && !world.never_freezes)
                ),
                _ => anyhow::bail!("no such file"),
            })
        }

        fn write(&self, cgroup: &str, file: &str, value: &str) -> anyhow::Result<()> {
            let mut world = self.0.lock().unwrap();
            anyhow::ensure!(world.nodes.contains_key(cgroup), "no cgroup");
            match file {
                "cgroup.freeze" => world.nodes.get_mut(cgroup).unwrap().frozen = value == "1",
                "cgroup.procs" => {
                    let pid: u32 = value.parse()?;
                    anyhow::ensure!(world.refuse_pid != Some(pid), "refused");
                    let from = world.procs[&pid].cgroup.clone();
                    world
                        .nodes
                        .get_mut(&from)
                        .unwrap()
                        .procs
                        .retain(|p| *p != pid);
                    world.nodes.get_mut(cgroup).unwrap().procs.push(pid);
                    world.procs.get_mut(&pid).unwrap().cgroup = cgroup.to_owned();
                }
                _ => anyhow::bail!("no such file"),
            }
            Ok(())
        }

        fn children(&self, cgroup: &str) -> anyhow::Result<Vec<String>> {
            let world = self.0.lock().unwrap();
            let node = world
                .nodes
                .get(cgroup)
                .ok_or_else(|| anyhow::anyhow!("no cgroup"))?;
            Ok(node.children.iter().cloned().collect())
        }
    }

    struct Held(Fake, u32);

    impl HeldProcess for Held {
        fn start_time(&self) -> u64 {
            1
        }
        fn cgroup(&self) -> anyhow::Result<String> {
            Ok(self.0.0.lock().unwrap().procs[&self.1].cgroup.clone())
        }
        fn ids(&self) -> anyhow::Result<ProcessIds> {
            let world = self.0.0.lock().unwrap();
            let process = &world.procs[&self.1];
            Ok(ProcessIds {
                uids: [process.uid; 4],
                gids: [process.gid; 4],
            })
        }
        fn oom_score_adj(&self) -> anyhow::Result<i32> {
            Ok(0)
        }
        fn set_oom_score_adj(&self, _: i32) -> anyhow::Result<()> {
            Ok(())
        }
    }

    impl ProcessAccess for Fake {
        fn open(&self, pid: u32, _: Option<u64>) -> anyhow::Result<Box<dyn HeldProcess>> {
            anyhow::ensure!(self.0.lock().unwrap().procs.contains_key(&pid), "gone");
            Ok(Box::new(Held(self.clone(), pid)))
        }
    }

    const ORDINARY: &str = "/cedegrid.slice/cedegrid-u1000.service";
    const IDLE: &str = "/cedegrid_idle.slice/cedegrid-idle-u1000.service";
    const CALLER: Caller = Caller {
        uid: 1000,
        primary_gid: 1100,
    };

    fn units() -> CallerUnits {
        CallerUnits {
            ordinary: ORDINARY.into(),
            idle: IDLE.into(),
        }
    }

    fn world() -> Fake {
        let fake = Fake::default();
        fake.cgroup(&format!("{ORDINARY}/jobs"), &[]);
        fake.cgroup(&format!("{IDLE}/jobs"), &[]);
        fake.cgroup(&format!("{ORDINARY}/jobs/cedegrid-neutral-j1"), &[11, 12]);
        fake.cgroup(&format!("{IDLE}/jobs/cedegrid-yield_first-j1"), &[]);
        fake
    }

    #[test]
    fn leaves_with_a_nonce_move_too() {
        let fake = Fake::default();
        fake.cgroup(&format!("{ORDINARY}/jobs"), &[]);
        fake.cgroup(&format!("{IDLE}/jobs"), &[]);
        fake.cgroup(
            &format!("{ORDINARY}/jobs/cedegrid-neutral-j1.0a0b0c0d0e0f1011"),
            &[21],
        );
        fake.cgroup(
            &format!("{IDLE}/jobs/cedegrid-yield_first-j1.1213141516171819"),
            &[],
        );
        let moved = move_job(&fake, &fake, CALLER, &units(), &job(), ModeName::YieldFirst).unwrap();
        assert_eq!(
            moved.to,
            format!("{IDLE}/jobs/cedegrid-yield_first-j1.1213141516171819")
        );
        assert_eq!(moved.pids, vec![21]);
    }

    fn job() -> JobId {
        JobId::parse("j1").unwrap()
    }

    #[test]
    fn a_job_moves_whole_and_the_source_is_thawed() {
        let fake = world();
        let moved = move_job(&fake, &fake, CALLER, &units(), &job(), ModeName::YieldFirst).unwrap();
        let source = format!("{ORDINARY}/jobs/cedegrid-neutral-j1");
        let target = format!("{IDLE}/jobs/cedegrid-yield_first-j1");
        assert_eq!(moved.pids, vec![11, 12]);
        assert_eq!(fake.procs(&target), vec![11, 12]);
        assert!(fake.procs(&source).is_empty());
        assert!(!fake.frozen(&source));
    }

    #[test]
    fn a_missing_job_or_target_is_refused() {
        let fake = world();
        let other = JobId::parse("j2").unwrap();
        assert_eq!(
            move_job(&fake, &fake, CALLER, &units(), &other, ModeName::YieldFirst)
                .unwrap_err()
                .code,
            RefusalCode::JobNotFound
        );
        // Moving to Neutral moves from the idle unit, where only the empty destination is.
        assert_eq!(
            move_job(&fake, &fake, CALLER, &units(), &job(), ModeName::Neutral)
                .unwrap_err()
                .code,
            RefusalCode::MoveRefused
        );
        let fake = world();
        fake.cgroup(&format!("{IDLE}/jobs/cedegrid-yield_first-j1"), &[99]);
        assert_eq!(
            move_job(&fake, &fake, CALLER, &units(), &job(), ModeName::YieldFirst)
                .unwrap_err()
                .code,
            RefusalCode::MoveRefused
        );
        assert_eq!(
            fake.procs(&format!("{ORDINARY}/jobs/cedegrid-neutral-j1")),
            vec![11, 12]
        );
    }

    #[test]
    fn another_users_process_in_the_job_refuses_the_move() {
        let fake = world();
        fake.0.lock().unwrap().procs.get_mut(&12).unwrap().uid = 0;
        let source = format!("{ORDINARY}/jobs/cedegrid-neutral-j1");
        assert_eq!(
            move_job(&fake, &fake, CALLER, &units(), &job(), ModeName::YieldFirst)
                .unwrap_err()
                .code,
            RefusalCode::MoveRefused
        );
        assert_eq!(fake.procs(&source), vec![11, 12]);
        assert!(!fake.frozen(&source));
    }

    #[test]
    fn a_failed_write_puts_moved_processes_back() {
        let fake = world();
        fake.0.lock().unwrap().refuse_pid = Some(12);
        let source = format!("{ORDINARY}/jobs/cedegrid-neutral-j1");
        assert_eq!(
            move_job(&fake, &fake, CALLER, &units(), &job(), ModeName::YieldFirst)
                .unwrap_err()
                .code,
            RefusalCode::MoveRefused
        );
        let mut back = fake.procs(&source);
        back.sort_unstable();
        assert_eq!(back, vec![11, 12]);
        assert!(!fake.frozen(&source));
    }

    #[test]
    fn a_job_with_child_cgroups_or_that_never_freezes_is_refused() {
        let fake = world();
        fake.cgroup(&format!("{ORDINARY}/jobs/cedegrid-neutral-j1/inner"), &[]);
        assert_eq!(
            move_job(&fake, &fake, CALLER, &units(), &job(), ModeName::YieldFirst)
                .unwrap_err()
                .code,
            RefusalCode::MoveRefused
        );
        let fake = world();
        fake.0.lock().unwrap().never_freezes = true;
        let source = format!("{ORDINARY}/jobs/cedegrid-neutral-j1");
        assert_eq!(
            move_job(&fake, &fake, CALLER, &units(), &job(), ModeName::YieldFirst)
                .unwrap_err()
                .code,
            RefusalCode::MoveRefused
        );
        assert!(!fake.frozen(&source));
        assert_eq!(fake.procs(&source), vec![11, 12]);
    }
}

// T016, T017, T029, T031, T035: container fixtures. They run only inside the system-mode
// image with systemd as PID 1 (tools/docker-validation/system-mode/run.sh), as root.

#[cfg(target_os = "linux")]
mod container {
    include!("support/system_container.rs");

    const GATE: &str =
        "not tested: requires the system-mode container (CEDEGRID_TEST_SYSTEM_CONTAINER)";

    fn stdout(output: &Output) -> String {
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn stderr(output: &Output) -> String {
        String::from_utf8_lossy(&output.stderr).into_owned()
    }

    fn ordinary(uid: u32) -> String {
        format!("cedegrid-u{uid}.service")
    }

    fn idle(uid: u32) -> String {
        format!("cedegrid-idle-u{uid}.service")
    }

    /// Runs `python3 -c <code>` as the member with the copied SDK on its path.
    fn python_as(user: &str, home: &str, base: &str, code: &str) -> Output {
        run(
            "runuser",
            &[
                "-u",
                user,
                "--",
                "env",
                &format!("HOME={home}"),
                &format!("PYTHONPATH={base}/python:{base}/tools"),
                "PYTHONDONTWRITEBYTECODE=1",
                &format!("TMPDIR={base}"),
                "python3",
                "-c",
                code,
            ],
        )
    }

    fn unit_log(uid: u32) -> String {
        stdout(&run(
            "journalctl",
            &["-u", &ordinary(uid), "-o", "cat", "--no-pager"],
        ))
    }

    /// 004 T019, T020: a member's unprotected grant goes through the agent in its unit and is
    /// refused while the administrator key or the allowance is missing, naming each, and starts
    /// once both hold, logging its warning. A started run keeps its grant across a drained and
    /// retried attempt after the allowance is turned off, and a new run is then refused.
    #[test]
    #[ignore = "not tested: requires the system-mode container (CEDEGRID_TEST_SYSTEM_CONTAINER)"]
    fn an_unprotected_grant_starts_only_under_the_policy_key_and_the_allowance() {
        use cedegrid::agent::AgentConfig;
        use cedegrid::config::{
            Config, NodeMode, RuntimeClientConfig, RuntimeConfigKind, serialize_runtime,
        };
        use cedegrid::contention::ContentionMode;
        use cedegrid::protocol::{CoordinatorConfig, TlsIdentity};
        use std::path::Path;
        assert!(enabled(), "{GATE}");
        install();
        let repo = Path::new(&std::env::var("CEDEGRID_TEST_PACKAGING").unwrap())
            .parent()
            .and_then(Path::parent)
            .unwrap()
            .to_path_buf();
        let home = format!("/home/{MEMBER}");
        let base = format!("{home}/agg");
        // Durable state needs a local disk filesystem, and the container's is an overlay: a
        // loop-mounted ext4 image holds every state directory of this fixture.
        let image = "/var/tmp/cedegrid-agg.img";
        let _ = run("umount", &[&base]);
        let _ = std::fs::remove_dir_all(&base);
        ok("truncate", &["-s", "256M", image]);
        ok("mkfs.ext4", &["-q", "-F", image]);
        std::fs::create_dir_all(&base).unwrap();
        // The fixture attaches the loop device itself and detaches it at the end: a device that
        // `mount -o loop` attaches can outlive the unmount and use up the container's few. A
        // privileged container sees only the loop nodes /dev had when it started, so the node
        // the kernel hands out next is made if it is missing.
        let free = ok("losetup", &["--find"]).trim().to_owned();
        if let Some(minor) = free.strip_prefix("/dev/loop")
            && !std::path::Path::new(&free).exists()
        {
            ok("mknod", &[&free, "b", "7", minor]);
        }
        let device = ok("losetup", &["--find", "--show", image])
            .trim()
            .to_owned();
        ok("mount", &[&device, &base]);
        std::fs::create_dir_all(format!("{base}/tools")).unwrap();
        std::fs::create_dir_all(format!("{base}/python")).unwrap();
        for tool in ["make_test_pki.py", "validation_runtime.py"] {
            std::fs::copy(
                repo.join("tools").join(tool),
                format!("{base}/tools/{tool}"),
            )
            .unwrap();
        }
        ok(
            "cp",
            &[
                "-r",
                repo.join("python/cedegrid").to_str().unwrap(),
                &format!("{base}/python/"),
            ],
        );
        ok("chown", &["-R", &format!("{MEMBER}:{MEMBER}"), &base]);
        let generated = python_as(
            MEMBER,
            &home,
            &base,
            &format!(
                "import runpy, sys; sys.argv=['make_test_pki.py','--node-id','node-a','--output','{base}/pki']; runpy.run_path('{base}/tools/make_test_pki.py', run_name='__main__')"
            ),
        );
        assert!(generated.status.success(), "{}", stderr(&generated));
        let info: serde_json::Value = serde_json::from_slice(&generated.stdout).unwrap();
        let tls = |name: &str| TlsIdentity {
            ca_cert: format!("{base}/pki/ca.pem").into(),
            certificate: format!("{base}/pki/{name}.pem").into(),
            private_key: format!("{base}/pki/{name}.key").into(),
        };
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let endpoint = format!("https://{address}");
        let coordinator: CoordinatorConfig = serde_json::from_value(serde_json::json!({
            "state_dir": format!("{base}/coordinator"), "listen": address, "tls": tls("server"),
            "clients": info["clients"], "lease_ms": 3000, "max_artifact_bytes": 1048576,
            "artifact_quota_bytes": 10485760,
        }))
        .unwrap();
        let write = |path: &str, text: String| {
            std::fs::write(path, text).unwrap();
            ok("chown", &[&format!("{MEMBER}:{MEMBER}"), path]);
        };
        write(
            &format!("{base}/coordinator.toml"),
            serialize_runtime(&coordinator, RuntimeConfigKind::Coordinator).unwrap(),
        );
        let operator = RuntimeClientConfig {
            endpoint: endpoint.clone(),
            tls: tls("operator"),
            timeout_seconds: 15.0,
            max_transfer_bytes_per_second: 1024 * 1024,
        };
        write(
            &format!("{base}/operator.toml"),
            serialize_runtime(&operator, RuntimeConfigKind::Client).unwrap(),
        );
        let mut node = Config {
            node_id: "node-a".into(),
            node_mode: NodeMode::Opportunistic,
            state_dir: format!("{base}/state").into(),
            ..Config::default()
        };
        node.execution.enabled = true;
        node.cpu.reserve_physical_cores = 0;
        node.ram.reserve_mib = 512;
        node.ram.reserve_percent = 0;
        node.monitor.interval_ms = 500;
        node.contention.default_mode = ContentionMode::Aggressive;
        node.contention.max_mode = Some(ContentionMode::Aggressive);
        node.contention
            .acknowledge_caution
            .insert("aggressive_mode".into());
        let agent = AgentConfig {
            coordinator_url: endpoint.clone(),
            tls: tls("node-0"),
            cpu_affinity: None,
            max_transfer_bytes_per_second: 1024 * 1024,
            capacity: cedegrid::model::Resources {
                cpu_millicores: 1000,
                ram_mib: 512,
                gpu_memory_mib: BTreeMap::new(),
            },
            max_workers: 1,
            max_spool_bytes: 10 * 1024 * 1024,
            max_runtime_seconds: 60,
        };
        let configs = format!("{home}/.config/cedegrid");
        std::fs::create_dir_all(&configs).unwrap();
        ok(
            "chown",
            &[
                "-R",
                &format!("{MEMBER}:{MEMBER}"),
                &format!("{home}/.config"),
            ],
        );
        write(
            &format!("{configs}/cedegrid.toml"),
            serialize_runtime(&node, RuntimeConfigKind::Node).unwrap(),
        );
        write(
            &format!("{configs}/deployment.toml"),
            serialize_runtime(&agent, RuntimeConfigKind::Agent).unwrap(),
        );
        // Aggressive is allowed; the unprotected key is absent, so false.
        let policy = |unprotected: bool| {
            ok("install", &["-d", "-m", "0755", "/etc/cedegrid"]);
            std::fs::write(
                "/etc/cedegrid/policy.toml",
                format!(
                    "policy_version = 1\n[modes]\nallowed = [\"yield_first\", \"neutral\", \"performance\", \"aggressive\"]\n{}",
                    if unprotected { "unprotected = true\n" } else { "" }
                ),
            )
            .unwrap();
            ok("chmod", &["0644", "/etc/cedegrid/policy.toml"]);
        };
        policy(false);
        let mut coordinator = Command::new("runuser")
            .args([
                "-u",
                MEMBER,
                "--",
                "/usr/bin/cedegrid",
                "coordinator",
                "--deployment",
            ])
            .arg(format!("{base}/coordinator.toml"))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        // Units an earlier fixture left holding without a configuration would be reused as
        // they are; start fresh ones that read the configuration written above.
        stop_units(MEMBER);
        let started = cedegrid_as(MEMBER, &["system", "start"]);
        let submit = |job: &str, seconds: u32, attempts: u32| {
            python_as(
                MEMBER,
                &home,
                &base,
                &format!(
                    r#"import time
from cedegrid import Client, command_task
c = Client.from_config('{base}/operator.toml')
deadline = time.time() + 60
while True:
    try:
        if c.status().get('nodes'): break
    except Exception:
        pass
    assert time.time() < deadline, 'the agent did not register'
    time.sleep(0.5)
c.put_pool('pool', ['node-a'], max_workers=1, min_workers=0, allocation_class='opportunistic')
t = command_task('{job}', ['/bin/sleep', '{seconds}'], '/tmp', cpu_millicores=100, ram_mib=64,
    allocation_class='opportunistic', single_process=True, no_escape=True, replay_safe=True,
    max_attempts={attempts}, unprotected=True, confirm_unprotected=True)
c.submit('{job}', 'pool', [t])
"#
                ),
            )
        };
        let refusals = || {
            unit_log(MEMBER_UID)
                .lines()
                .filter(|line| line.contains("\"unprotected_refused\""))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        let state = |job: &str| {
            stdout(&python_as(
                MEMBER,
                &home,
                &base,
                &format!(
                    "from cedegrid import Client\nc = Client.from_config('{base}/operator.toml')\nprint([t['status'] for t in c.status('{job}').get('tasks', [])])"
                ),
            ))
        };
        let outcome = (|| -> Result<(), String> {
            if !started.status.success() {
                return Err(format!("system start: {}", stderr(&started)));
            }
            // 1. The key and the allowance are both missing: refused, naming both.
            let sent = submit("job-a", 1, 1);
            if !sent.status.success() {
                let log = unit_log(MEMBER_UID);
                let tail: Vec<&str> = log.lines().rev().take(15).collect();
                return Err(format!("submit a: {}\nunit log: {tail:?}", stderr(&sent)));
            }
            if !wait_until(Duration::from_secs(60), || !refusals().is_empty()) {
                return Err(format!("no refusal for job-a: {}", unit_log(MEMBER_UID)));
            }
            let first = refusals()[0].clone();
            for needle in [
                "ERR_CEDEGRID_UNPROTECTED_POLICY",
                "ERR_CEDEGRID_UNPROTECTED_NOT_ALLOWED",
            ] {
                if !first.contains(needle) {
                    return Err(format!("{needle} missing from {first}"));
                }
            }
            // 2. The allowance is on (as setup records it); the key is still missing.
            let set = python_as(
                MEMBER,
                &home,
                &base,
                &format!(
                    "import sqlite3, time\nc = sqlite3.connect('{base}/state/state.sqlite3')\nc.execute('CREATE TABLE IF NOT EXISTS unprotected_allowance (id INTEGER PRIMARY KEY CHECK (id = 1), enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)), set_at_unix_ms INTEGER, set_by TEXT NOT NULL)')\nc.execute('INSERT OR REPLACE INTO unprotected_allowance VALUES (1, 1, ?, ?)', (int(time.time() * 1000), 'interactive_setup'))\nc.commit()"
                ),
            );
            if !set.status.success() {
                return Err(format!("allowance: {}", stderr(&set)));
            }
            let sent = submit("job-b", 1, 1);
            if !sent.status.success() {
                return Err(format!("submit b: {}", stderr(&sent)));
            }
            if !wait_until(Duration::from_secs(60), || {
                refusals().iter().any(|line| {
                    line.contains("ERR_CEDEGRID_UNPROTECTED_POLICY")
                        && !line.contains("ERR_CEDEGRID_UNPROTECTED_NOT_ALLOWED")
                })
            }) {
                return Err(format!("no policy-only refusal: {:?}", refusals()));
            }
            // 3. The key permits it: a new grant starts, logs its warning and completes. The
            // assignments refused above were left to the coordinator, as every launch refusal is.
            policy(true);
            let sent = submit("job-c", 1, 1);
            if !sent.status.success() {
                return Err(format!("submit c: {}", stderr(&sent)));
            }
            if !wait_until(Duration::from_secs(90), || {
                state("job-c").contains("completed")
            }) {
                return Err(format!(
                    "job-c did not complete: {} {}",
                    state("job-c"),
                    unit_log(MEMBER_UID)
                ));
            }
            let warnings = || {
                unit_log(MEMBER_UID)
                    .matches("this run is UNPROTECTED")
                    .count()
            };
            if warnings() == 0 {
                return Err("no warning line for the started grant".into());
            }
            // 4. A started run keeps its grant across a drained and retried attempt after the
            // allowance is turned off; a new run is refused (004 edge cases).
            let operator = |code: &str| {
                python_as(
                    MEMBER,
                    &home,
                    &base,
                    &format!(
                        "from cedegrid import Client\nc = Client.from_config('{base}/operator.toml')\n{code}"
                    ),
                )
            };
            let started = warnings();
            let sent = submit("job-d", 30, 3);
            if !sent.status.success() {
                return Err(format!("submit d: {}", stderr(&sent)));
            }
            if !wait_until(Duration::from_secs(60), || {
                warnings() > started && state("job-d").contains("assigned")
            }) {
                return Err(format!("job-d did not start: {}", state("job-d")));
            }
            // Status marks the started run unprotected, with its grant (004 FR-016).
            let marks = || {
                stdout(&operator(
                    "print([a.get('unprotected') for a in c.status('job-d').get('allocations', [])])",
                ))
            };
            // The warning comes at the grant check; the mark follows authorization.
            if !wait_until(Duration::from_secs(30), || marks().contains("True")) {
                return Err(format!(
                    "status does not mark job-d unprotected: {}",
                    marks()
                ));
            }
            let cleared = python_as(
                MEMBER,
                &home,
                &base,
                &format!(
                    "import sqlite3\nc = sqlite3.connect('{base}/state/state.sqlite3')\nc.execute('DELETE FROM unprotected_allowance')\nc.commit()"
                ),
            );
            if !cleared.status.success() {
                return Err(format!("allowance off: {}", stderr(&cleared)));
            }
            let refused = refusals().len();
            let drained = operator("c.drain_node('node-a')");
            if !drained.status.success() {
                return Err(format!("drain: {}", stderr(&drained)));
            }
            if !wait_until(Duration::from_secs(60), || {
                !state("job-d").contains("assigned")
            }) {
                return Err(format!("job-d was not drained: {}", state("job-d")));
            }
            let resumed = operator("c.drain_node('node-a', False)");
            if !resumed.status.success() {
                return Err(format!("undrain: {}", stderr(&resumed)));
            }
            if !wait_until(Duration::from_secs(120), || {
                state("job-d").contains("completed")
            }) {
                return Err(format!(
                    "job-d did not complete after its retry: {} {:?}",
                    state("job-d"),
                    refusals()
                ));
            }
            // The mark ends with the run, once its node reports the allocation released.
            if !wait_until(Duration::from_secs(30), || !marks().contains("True")) {
                return Err(format!("job-d is still marked after it ended: {}", marks()));
            }
            if warnings() < started + 2 || refusals().len() != refused {
                return Err(format!(
                    "the retry did not keep its grant: {} warnings, refusals {:?}",
                    warnings() - started,
                    &refusals()[refused..]
                ));
            }
            let sent = submit("job-e", 1, 1);
            if !sent.status.success() {
                return Err(format!("submit e: {}", stderr(&sent)));
            }
            if !wait_until(Duration::from_secs(60), || {
                refusals()[refused..].iter().any(|line| {
                    line.contains("ERR_CEDEGRID_UNPROTECTED_NOT_ALLOWED")
                        && !line.contains("ERR_CEDEGRID_UNPROTECTED_POLICY")
                })
            }) {
                return Err(format!(
                    "job-e was not refused: {:?}",
                    &refusals()[refused..]
                ));
            }
            Ok(())
        })();
        stop_units(MEMBER);
        // runuser keeps a PAM session open around its child, so killing it leaves the
        // coordinator running and the image busy; end the coordinator itself.
        let _ = run(
            "pkill",
            &[
                "-KILL",
                "-u",
                MEMBER,
                "-f",
                &format!("cedegrid coordinator --deployment {base}/"),
            ],
        );
        let _ = coordinator.kill();
        let _ = coordinator.wait();
        let _ = std::fs::remove_file("/etc/cedegrid/policy.toml");
        let _ = run("umount", &[&base]);
        let _ = run("losetup", &["-d", &device]);
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_file(image);
        let _ = std::fs::remove_dir_all(&configs);
        outcome.unwrap();
    }

    #[test]
    #[ignore = "not tested: requires the system-mode container (CEDEGRID_TEST_SYSTEM_CONTAINER)"]
    fn socket_is_root_group_cedegrid_0660() {
        assert!(enabled(), "{GATE}");
        install();
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let meta = std::fs::metadata(cedegrid::broker::SOCKET_PATH).unwrap();
        let group = ok("getent", &["group", "cedegrid"]);
        let gid: u32 = group.split(':').nth(2).unwrap().parse().unwrap();
        assert_eq!(meta.uid(), 0);
        assert_eq!(meta.gid(), gid);
        assert_eq!(meta.permissions().mode() & 0o7777, 0o660);
        // A non-member cannot connect, so it continues without System mode.
        let output = cedegrid_as(OUTSIDER, &["system", "status"]);
        assert!(output.status.success(), "{}", stderr(&output));
        assert!(stdout(&output).contains(r#""system":false"#));
        assert!(
            stderr(&output).contains("not a member"),
            "{}",
            stderr(&output)
        );
        let output = cedegrid_as(OUTSIDER, &["system", "start", "--require-system"]);
        assert!(!output.status.success());
    }

    /// 006 T043, FR-016, FR-018: under `modes.unprotected` a member's raise weights its own
    /// account slice and nothing else. It is refused without the key, where the ordinary unit
    /// is not in the account slice, and over a weight the broker did not set. Withdrawal, the
    /// units' stop, the broker's stop, the next broker's start and an unrenewed lease each end it.
    #[test]
    #[ignore = "not tested: requires the system-mode container (CEDEGRID_TEST_SYSTEM_CONTAINER)"]
    fn an_unprotected_raise_weights_only_the_account_slice_and_every_end_resets_it() {
        assert!(enabled(), "{GATE}");
        install();
        stop_units(MEMBER);
        let slice = format!("user-{MEMBER_UID}.slice");
        let holder = "raise-fixture-split.service";
        let cleanup = || {
            let _ = run("systemctl", &["stop", holder]);
            stop_units(MEMBER);
            let _ = std::fs::remove_file("/etc/cedegrid/policy.toml");
            let _ = run("systemctl", &["revert", "--", &slice]);
        };
        cleanup();
        let outcome = std::panic::catch_unwind(|| {
            let weight = || show(&slice, "CPUWeight")["CPUWeight"].clone();
            let raise = |on: bool| {
                reply_code(&raw_request(
                    Some(MEMBER),
                    format!("{{\"protocol\":1,\"kind\":\"raise\",\"on\":{on}}}\n").as_bytes(),
                    false,
                ))
            };
            let policy = |unprotected: bool| {
                ok("install", &["-d", "-m", "0755", "/etc/cedegrid"]);
                std::fs::write(
                    "/etc/cedegrid/policy.toml",
                    format!("policy_version = 1\n[modes]\nunprotected = {unprotected}\n"),
                )
                .unwrap();
                ok("chmod", &["0644", "/etc/cedegrid/policy.toml"]);
            };
            let start = || {
                let output = cedegrid_as(MEMBER, &["system", "start"]);
                assert!(output.status.success(), "{}", stderr(&output));
                assert!(wait_until(Duration::from_secs(10), || {
                    show(&ordinary(MEMBER_UID), "ActiveState")["ActiveState"] == "active"
                }));
                show(&ordinary(MEMBER_UID), "Slice")["Slice"].clone()
            };
            let unset = "[not set]";
            // Without a split by account the ordinary unit sits in cedegrid.slice, where a raise
            // would reach no other account.
            policy(true);
            assert_eq!(start(), "cedegrid.slice");
            assert_eq!(raise(true), "raise_refused");
            stop_units(MEMBER);
            // Another account's work with a CPU weight splits user.slice by account, as on a
            // desktop host; the ordinary unit then sits in the account slice.
            ok(
                "systemd-run",
                &[
                    "--quiet",
                    "--unit",
                    holder,
                    "--slice",
                    &format!("user-{SECOND_UID}.slice"),
                    "-p",
                    "CPUWeight=100",
                    "sleep",
                    "600",
                ],
            );
            assert!(wait_until(Duration::from_secs(5), || {
                std::fs::read_to_string("/sys/fs/cgroup/user.slice/cgroup.subtree_control")
                    .is_ok_and(|text| text.split_whitespace().any(|c| c == "cpu"))
            }));
            policy(false);
            assert_eq!(start(), slice);
            assert_eq!(raise(true), "raise_refused");
            assert_eq!(weight(), unset);
            // Permitted: the account slice, and no other, at the default weight, read back.
            policy(true);
            assert_eq!(raise(true), "ok");
            assert_eq!(weight(), "400");
            assert_eq!(
                show(&format!("user-{SECOND_UID}.slice"), "CPUWeight")["CPUWeight"],
                unset
            );
            assert_eq!(raise(false), "ok");
            assert_eq!(weight(), unset);
            // The administrator's key off ends it at the broker's next check, within 5 s.
            assert_eq!(raise(true), "ok");
            policy(false);
            assert!(
                wait_until(Duration::from_secs(8), || weight() == unset),
                "{}",
                weight()
            );
            policy(true);
            // The units' stop ends it.
            assert_eq!(raise(true), "ok");
            stop_units(MEMBER);
            assert_eq!(weight(), unset);
            // So does the broker's stop, through its unit's stop hook.
            assert_eq!(start(), slice);
            assert_eq!(raise(true), "ok");
            ok("systemctl", &["stop", "cedegrid-broker.service"]);
            assert!(
                wait_until(Duration::from_secs(10), || weight() == unset),
                "{}",
                weight()
            );
            // A raise left without the hook ends at the next broker's start.
            ok(
                "systemctl",
                &["set-property", "--runtime", "--", &slice, "CPUWeight=400"],
            );
            assert_eq!(
                reply_code(&raw_request(
                    Some(MEMBER),
                    b"{\"protocol\":1,\"kind\":\"status\"}\n",
                    false
                )),
                "ok"
            );
            assert_eq!(weight(), unset);
            // An administrator's weight is never replaced, and outlives the broker's stop.
            ok(
                "systemctl",
                &["set-property", "--runtime", "--", &slice, "CPUWeight=250"],
            );
            assert_eq!(raise(true), "raise_refused");
            assert_eq!(weight(), "250");
            ok("systemctl", &["stop", "cedegrid-broker.service"]);
            assert_eq!(weight(), "250");
            ok(
                "systemctl",
                &["set-property", "--runtime", "--", &slice, "CPUWeight="],
            );
            // A lease nobody renews ends within its 30 s and the 5 s check.
            assert_eq!(raise(true), "ok");
            assert_eq!(weight(), "400");
            assert!(
                wait_until(Duration::from_secs(45), || weight() == unset),
                "{}",
                weight()
            );
        });
        cleanup();
        outcome.unwrap();
    }

    /// 006 T072, FR-027: under `modes.plain_priority` a member's plain raise weights its own
    /// account slice at `modes.plain_priority_cpu_weight` and nothing else. It is refused without
    /// the key; the unprotected level stays refused without its own key and leaves the plain
    /// weight alone. Withdrawal, the key turning off, the broker's stop and the next broker's start
    /// each end it.
    #[test]
    #[ignore = "not tested: requires the system-mode container (CEDEGRID_TEST_SYSTEM_CONTAINER)"]
    fn a_plain_raise_weights_only_the_account_slice_at_its_own_weight() {
        assert!(enabled(), "{GATE}");
        install();
        stop_units(MEMBER);
        let slice = format!("user-{MEMBER_UID}.slice");
        let holder = "raise-fixture-plain-split.service";
        let cleanup = || {
            let _ = run("systemctl", &["stop", holder]);
            stop_units(MEMBER);
            let _ = std::fs::remove_file("/etc/cedegrid/policy.toml");
            let _ = run("systemctl", &["revert", "--", &slice]);
        };
        cleanup();
        let outcome = std::panic::catch_unwind(|| {
            let weight = || show(&slice, "CPUWeight")["CPUWeight"].clone();
            let raise = |on: bool, level: &str| {
                reply_code(&raw_request(
                    Some(MEMBER),
                    format!(
                        "{{\"protocol\":1,\"kind\":\"raise\",\"on\":{on},\"level\":\"{level}\"}}\n"
                    )
                    .as_bytes(),
                    false,
                ))
            };
            let policy = |plain: bool| {
                ok("install", &["-d", "-m", "0755", "/etc/cedegrid"]);
                std::fs::write(
                    "/etc/cedegrid/policy.toml",
                    format!(
                        "policy_version = 1\n[modes]\nplain_priority = {plain}\nplain_priority_cpu_weight = 300\n"
                    ),
                )
                .unwrap();
                ok("chmod", &["0644", "/etc/cedegrid/policy.toml"]);
            };
            let start = || {
                let output = cedegrid_as(MEMBER, &["system", "start"]);
                assert!(output.status.success(), "{}", stderr(&output));
                assert!(wait_until(Duration::from_secs(10), || {
                    show(&ordinary(MEMBER_UID), "ActiveState")["ActiveState"] == "active"
                }));
                show(&ordinary(MEMBER_UID), "Slice")["Slice"].clone()
            };
            let unset = "[not set]";
            // Another account's work with a CPU weight splits user.slice by account.
            ok(
                "systemd-run",
                &[
                    "--quiet",
                    "--unit",
                    holder,
                    "--slice",
                    &format!("user-{SECOND_UID}.slice"),
                    "-p",
                    "CPUWeight=100",
                    "sleep",
                    "600",
                ],
            );
            assert!(wait_until(Duration::from_secs(5), || {
                std::fs::read_to_string("/sys/fs/cgroup/user.slice/cgroup.subtree_control")
                    .is_ok_and(|text| text.split_whitespace().any(|c| c == "cpu"))
            }));
            policy(false);
            assert_eq!(start(), slice);
            assert_eq!(raise(true, "plain"), "raise_refused");
            assert_eq!(weight(), unset);
            // Permitted: the account slice, and no other, at the plain weight, read back.
            policy(true);
            assert_eq!(raise(true, "plain"), "ok");
            assert_eq!(weight(), "300");
            assert_eq!(
                show(&format!("user-{SECOND_UID}.slice"), "CPUWeight")["CPUWeight"],
                unset
            );
            // The unprotected level has its own key, and a refusal leaves the weight.
            assert_eq!(raise(true, "unprotected"), "raise_refused");
            assert_eq!(weight(), "300");
            assert_eq!(raise(false, "plain"), "ok");
            assert_eq!(weight(), unset);
            // The key off ends it at the broker's next check, within 5 s.
            assert_eq!(raise(true, "plain"), "ok");
            policy(false);
            assert!(
                wait_until(Duration::from_secs(8), || weight() == unset),
                "{}",
                weight()
            );
            policy(true);
            // The broker's stop ends it through its unit's stop hook.
            assert_eq!(raise(true, "plain"), "ok");
            ok("systemctl", &["stop", "cedegrid-broker.service"]);
            assert!(
                wait_until(Duration::from_secs(10), || weight() == unset),
                "{}",
                weight()
            );
            // A plain raise left without the hook ends at the next broker's start.
            ok(
                "systemctl",
                &["set-property", "--runtime", "--", &slice, "CPUWeight=300"],
            );
            assert_eq!(
                reply_code(&raw_request(
                    Some(MEMBER),
                    b"{\"protocol\":1,\"kind\":\"status\"}\n",
                    false
                )),
                "ok"
            );
            assert_eq!(weight(), unset);
        });
        cleanup();
        outcome.unwrap();
    }

    #[test]
    #[ignore = "not tested: requires the system-mode container (CEDEGRID_TEST_SYSTEM_CONTAINER)"]
    fn a_member_start_makes_both_units_with_the_contract_properties() {
        assert!(enabled(), "{GATE}");
        install();
        stop_units(MEMBER);
        let output = cedegrid_as(MEMBER, &["system", "start"]);
        assert!(output.status.success(), "{}", stderr(&output));
        assert!(
            stdout(&output).contains(r#""system":true"#),
            "{}",
            stdout(&output)
        );
        for unit in [ordinary(MEMBER_UID), idle(MEMBER_UID)] {
            assert!(
                wait_until(
                    Duration::from_secs(10),
                    || show(&unit, "ActiveState")["ActiveState"] == "active"
                ),
                "{unit} not active: {:?}",
                show(&unit, "ActiveState,SubState,Result")
            );
            let props = show(
                &unit,
                "User,Group,Delegate,TasksMax,OOMScoreAdjust,Restart,Environment,Description,ExecStart,Slice",
            );
            assert_eq!(props["User"], MEMBER_UID.to_string(), "{unit}");
            assert_eq!(props["Delegate"], "yes", "{unit}");
            assert_eq!(props["TasksMax"], "4096", "{unit}");
            assert_eq!(props["OOMScoreAdjust"], "0", "{unit}");
            assert_eq!(props["Restart"], "on-failure", "{unit}");
            assert!(props["Description"].starts_with("CedeGrid System mode unit"));
            assert!(
                props["Environment"].contains("CEDEGRID_SYSTEM_UNIT="),
                "{props:?}"
            );
            assert!(
                props["ExecStart"].contains("/usr/bin/cedegrid"),
                "{props:?}"
            );
        }
        assert_eq!(
            show(&idle(MEMBER_UID), "Slice")["Slice"],
            "cedegrid_idle.slice"
        );
        let slice = show(&ordinary(MEMBER_UID), "Slice")["Slice"].clone();
        assert!(
            slice == format!("user-{MEMBER_UID}.slice") || slice == "cedegrid.slice",
            "{slice}"
        );
        assert_eq!(
            show(&ordinary(MEMBER_UID), "RuntimeDirectory")["RuntimeDirectory"],
            format!("cedegrid/u{MEMBER_UID}")
        );
        // A second start leaves the running units alone.
        let before = show(&ordinary(MEMBER_UID), "InvocationID")["InvocationID"].clone();
        assert!(cedegrid_as(MEMBER, &["system", "start"]).status.success());
        assert_eq!(
            show(&ordinary(MEMBER_UID), "InvocationID")["InvocationID"],
            before
        );
        let status = cedegrid_as(MEMBER, &["system", "status"]);
        assert!(
            stdout(&status).contains("\"state\":\"active\""),
            "{}",
            stdout(&status)
        );
        let stopped = cedegrid_as(MEMBER, &["system", "stop"]);
        assert!(stopped.status.success(), "{}", stderr(&stopped));
        assert!(wait_until(Duration::from_secs(10), || {
            show(&ordinary(MEMBER_UID), "ActiveState")["ActiveState"] != "active"
        }));
    }

    #[test]
    #[ignore = "not tested: requires the system-mode container (CEDEGRID_TEST_SYSTEM_CONTAINER)"]
    fn two_members_get_their_own_units_and_starts_at_once_start_each_once() {
        assert!(enabled(), "{GATE}");
        install();
        stop_units(MEMBER);
        stop_units(SECOND);
        let first = std::thread::spawn(|| cedegrid_as(SECOND, &["system", "start"]));
        let second = std::thread::spawn(|| cedegrid_as(SECOND, &["system", "start"]));
        let (first, second) = (first.join().unwrap(), second.join().unwrap());
        assert!(
            stdout(&first).contains(r#""system":true"#)
                || stdout(&second).contains(r#""system":true"#),
            "{} / {}",
            stderr(&first),
            stderr(&second)
        );
        assert!(wait_until(Duration::from_secs(10), || {
            show(&ordinary(SECOND_UID), "ActiveState")["ActiveState"] == "active"
        }));
        assert_eq!(show(&ordinary(SECOND_UID), "NRestarts")["NRestarts"], "0");
        // The first member's status does not list the second member's units.
        let status = stdout(&cedegrid_as(MEMBER, &["system", "status"]));
        assert!(!status.contains(&SECOND_UID.to_string()), "{status}");
        stop_units(SECOND);
    }

    #[test]
    #[ignore = "not tested: requires the system-mode container (CEDEGRID_TEST_SYSTEM_CONTAINER)"]
    fn a_removed_members_units_stop_once_no_job_runs() {
        assert!(enabled(), "{GATE}");
        install();
        assert!(cedegrid_as(SECOND, &["system", "start"]).status.success());
        let (unit, idle_unit) = (ordinary(SECOND_UID), idle(SECOND_UID));
        assert!(wait_until(Duration::from_secs(10), || {
            show(&unit, "ActiveState")["ActiveState"] == "active"
        }));
        ok("gpasswd", &["-d", SECOND, "cedegrid"]);
        let refused = cedegrid_as(SECOND, &["system", "status", "--require-system"]);
        // N2 A: no job runs in either unit, so the broker's next full pass stops both.
        let stopped = wait_until(Duration::from_secs(45), || {
            [&unit, &idle_unit]
                .iter()
                .all(|name| show(name, "ActiveState")["ActiveState"] != "active")
        });
        ok("usermod", &["-aG", "cedegrid", SECOND]);
        assert!(!refused.status.success(), "{}", stdout(&refused));
        assert!(stopped, "a removed member's idle units still run");
    }

    #[test]
    #[ignore = "not tested: requires the system-mode container (CEDEGRID_TEST_SYSTEM_CONTAINER)"]
    fn an_upgrade_restarts_a_running_broker_and_keeps_the_units() {
        assert!(enabled(), "{GATE}");
        install();
        assert!(cedegrid_as(MEMBER, &["system", "start"]).status.success());
        let unit = ordinary(MEMBER_UID);
        assert!(wait_until(Duration::from_secs(10), || {
            show(&unit, "ActiveState")["ActiveState"] == "active"
        }));
        let invocation = show(&unit, "InvocationID")["InvocationID"].clone();
        let broker = show("cedegrid-broker.service", "MainPID")["MainPID"].clone();
        assert_ne!(broker, "0", "the start left no broker running");
        upgrade();
        let after = show("cedegrid-broker.service", "MainPID")["MainPID"].clone();
        assert_ne!(after, "0", "the upgrade left no broker running");
        assert_ne!(after, broker, "the upgrade kept the old broker process");
        assert_eq!(show(&unit, "ActiveState")["ActiveState"], "active");
        assert_eq!(show(&unit, "InvocationID")["InvocationID"], invocation);
        stop_units(MEMBER);
    }

    #[test]
    #[ignore = "not tested: requires the system-mode container (CEDEGRID_TEST_SYSTEM_CONTAINER)"]
    fn a_silent_broker_falls_back_and_running_units_survive_a_restart() {
        assert!(enabled(), "{GATE}");
        install();
        assert!(cedegrid_as(MEMBER, &["system", "start"]).status.success());
        let unit = ordinary(MEMBER_UID);
        assert!(wait_until(Duration::from_secs(10), || {
            show(&unit, "ActiveState")["ActiveState"] == "active"
        }));
        let invocation = show(&unit, "InvocationID")["InvocationID"].clone();
        ok("systemctl", &["restart", "cedegrid-broker.service"]);
        assert_eq!(show(&unit, "ActiveState")["ActiveState"], "active");
        assert_eq!(show(&unit, "InvocationID")["InvocationID"], invocation);
        // With the service masked, the socket stays but nothing answers.
        ok(
            "systemctl",
            &["mask", "--runtime", "cedegrid-broker.service"],
        );
        ok("systemctl", &["stop", "cedegrid-broker.service"]);
        let output = cedegrid_as(MEMBER, &["system", "status"]);
        ok(
            "systemctl",
            &["unmask", "--runtime", "cedegrid-broker.service"],
        );
        assert!(output.status.success(), "{}", stderr(&output));
        assert!(stdout(&output).contains(r#""system":false"#));
        assert!(
            stderr(&output).starts_with("cedegrid: System mode unavailable"),
            "{}",
            stderr(&output)
        );
        assert_eq!(stderr(&output).lines().count(), 1);
        stop_units(MEMBER);
    }

    #[test]
    #[ignore = "not tested: requires the system-mode container (CEDEGRID_TEST_SYSTEM_CONTAINER)"]
    fn hostile_requests_are_refused_logged_and_change_nothing() {
        assert!(enabled(), "{GATE}");
        install();
        stop_units(MEMBER);
        let status = br#"{"protocol":1,"kind":"status"}"#;
        for field in [
            "uid",
            "user",
            "unit",
            "path",
            "cgroup",
            "exec",
            "mount",
            "device",
            "capability",
        ] {
            let body = format!("{{\"protocol\":1,\"kind\":\"start\",\"{field}\":0}}\n");
            let reply = raw_request(Some(MEMBER), body.as_bytes(), false);
            assert_eq!(
                reply_code(&reply),
                "identity_in_request",
                "{field}: {reply}"
            );
        }
        for body in [
            &b"{\"protocol\":1,\"kind\":\"status\",\"a\\nb\":1}\n"[..],
            b"{\"protocol\":1,\"kind\":\"../../etc\"}\n",
            b"not json\n",
        ] {
            let reply = raw_request(Some(MEMBER), body, false);
            assert!(
                ["unknown_field", "unknown_kind", "malformed"]
                    .contains(&reply_code(&reply).as_str()),
                "{reply}"
            );
        }
        // 65,536 bytes with the newline are read; one more is refused.
        let mut at_limit = status.to_vec();
        at_limit.resize(65_535, b' ');
        at_limit.push(b'\n');
        assert_eq!(
            reply_code(&raw_request(Some(MEMBER), &at_limit, false)),
            "ok"
        );
        let mut over = status.to_vec();
        over.resize(65_536, b' ');
        over.push(b'\n');
        assert_eq!(
            reply_code(&raw_request(Some(MEMBER), &over, false)),
            "request_too_large"
        );
        // A message that never ends is cut off after the read timeout.
        let started = Instant::now();
        let reply = raw_request(Some(MEMBER), br#"{"protocol":1,"#, true);
        assert_eq!(reply_code(&reply), "request_timeout", "{reply}");
        assert!(started.elapsed() < Duration::from_secs(5));
        // Root is refused; a non-member cannot connect.
        assert_eq!(
            reply_code(&raw_request(
                None,
                b"{\"protocol\":1,\"kind\":\"status\"}\n",
                false
            )),
            "refused_account"
        );
        assert_eq!(
            raw_request(
                Some(OUTSIDER),
                b"{\"protocol\":1,\"kind\":\"status\"}\n",
                false
            ),
            "EACCES"
        );
        // Inside a user namespace where the member maps to 0, the broker still sees the member.
        // Hosts that forbid unprivileged user namespaces skip this part, and say so.
        let probe = run("runuser", &["-u", MEMBER, "--", "unshare", "-r", "true"]);
        if probe.status.success() {
            let output = run(
                "runuser",
                &[
                    "-u",
                    MEMBER,
                    "--",
                    "unshare",
                    "-r",
                    "/usr/bin/cedegrid",
                    "system",
                    "status",
                ],
            );
            // The client sees itself as root there and refuses before it connects.
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("refusing to run as root"),
                "{output:?}"
            );
            let raw = run(
                "runuser",
                &[
                    "-u",
                    MEMBER,
                    "--",
                    "unshare",
                    "-r",
                    "python3",
                    "-c",
                    "import socket;s=socket.socket(socket.AF_UNIX);s.connect('/run/cedegrid/broker.sock');s.sendall(b'{\"protocol\":1,\"kind\":\"status\"}\\n');print(s.recv(65536).decode().strip())",
                ],
            );
            assert_eq!(
                reply_code(String::from_utf8_lossy(&raw.stdout).trim()),
                "ok",
                "{raw:?}"
            );
        } else {
            eprintln!(
                "not tested here: unprivileged user namespaces are not permitted ({})",
                String::from_utf8_lossy(&probe.stderr).trim()
            );
        }
        // Nothing was started, and every refusal is in the journal.
        assert_ne!(
            show(&format!("cedegrid-u{MEMBER_UID}.service"), "ActiveState")["ActiveState"],
            "active"
        );
        let journal = ok(
            "journalctl",
            &["-u", "cedegrid-broker.service", "--no-pager", "-o", "cat"],
        );
        for code in [
            "identity_in_request",
            "request_too_large",
            "request_timeout",
            "refused_account",
        ] {
            assert!(journal.contains(&format!("result={code}")), "{code}");
        }
    }

    #[test]
    #[ignore = "not tested: requires the system-mode container (CEDEGRID_TEST_SYSTEM_CONTAINER)"]
    fn one_member_cannot_reach_another_members_units() {
        assert!(enabled(), "{GATE}");
        install();
        assert!(cedegrid_as(SECOND, &["system", "start"]).status.success());
        let unit = format!("cedegrid-u{SECOND_UID}.service");
        assert!(wait_until(Duration::from_secs(10), || {
            show(&unit, "ActiveState")["ActiveState"] == "active"
        }));
        let invocation = show(&unit, "InvocationID")["InvocationID"].clone();
        for body in [
            format!("{{\"protocol\":1,\"kind\":\"stop\",\"uid\":{SECOND_UID}}}\n"),
            format!("{{\"protocol\":1,\"kind\":\"stop\",\"user\":\"{SECOND}\"}}\n"),
            format!("{{\"protocol\":1,\"kind\":\"stop\",\"unit\":\"{unit}\"}}\n"),
            format!(
                "{{\"protocol\":1,\"kind\":\"move\",\"job\":\"x\",\"mode\":\"neutral\",\"cgroup\":\"/cedegrid.slice/{unit}\"}}\n"
            ),
        ] {
            let reply = raw_request(Some(MEMBER), body.as_bytes(), false);
            assert_eq!(reply_code(&reply), "identity_in_request", "{reply}");
        }
        // A plain stop stops only the caller's own units.
        let reply = raw_request(Some(MEMBER), b"{\"protocol\":1,\"kind\":\"stop\"}\n", false);
        assert_eq!(reply_code(&reply), "ok", "{reply}");
        assert_eq!(show(&unit, "ActiveState")["ActiveState"], "active");
        assert_eq!(show(&unit, "InvocationID")["InvocationID"], invocation);
        stop_units(SECOND);
    }

    #[test]
    #[ignore = "not tested: requires the system-mode container (CEDEGRID_TEST_SYSTEM_CONTAINER)"]
    fn a_flooding_member_is_rate_limited_and_others_are_still_served() {
        assert!(enabled(), "{GATE}");
        install();
        let status = b"{\"protocol\":1,\"kind\":\"status\"}\n";
        let flood: Vec<_> = (0..40)
            .map(|_| {
                std::thread::spawn(move || {
                    reply_code(&raw_request_once(Some(MEMBER), status, false))
                })
            })
            .collect();
        std::thread::sleep(Duration::from_millis(100));
        let started = Instant::now();
        let other = reply_code(&raw_request(Some(SECOND), status, false));
        let waited = started.elapsed();
        let codes: Vec<String> = flood.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(other, "ok");
        assert!(waited < Duration::from_secs(1), "{waited:?}");
        assert!(codes.iter().any(|code| code == "rate_limited"), "{codes:?}");
    }

    #[test]
    #[ignore = "not tested: requires the system-mode container (CEDEGRID_TEST_SYSTEM_CONTAINER)"]
    fn a_score_written_as_root_is_the_floor_for_the_user() {
        assert!(enabled(), "{GATE}");
        install();
        let mut child = Command::new("runuser")
            .args(["-u", MEMBER, "--", "sleep", "30"])
            .spawn()
            .unwrap();
        // runuser forks; the member's sleep is its child.
        let pid = wait_for_child(child.id(), "sleep");
        let handle = cedegrid::broker::process_handle::ProcessHandle::open(pid, None).unwrap();
        handle.set_oom_score_adj(-200).unwrap();
        let write_as_member = |value: i32| {
            run(
                "runuser",
                &[
                    "-u",
                    MEMBER,
                    "--",
                    "sh",
                    "-c",
                    &format!("echo {value} > /proc/{pid}/oom_score_adj"),
                ],
            )
            .status
            .success()
        };
        // The kernel refuses a lower value from the user and accepts a higher one.
        assert!(!write_as_member(-300));
        assert_eq!(handle.oom_score_adj().unwrap(), -200);
        assert!(write_as_member(-100));
        assert_eq!(handle.oom_score_adj().unwrap(), -100);
        child.kill().unwrap();
        let _ = child.wait();
    }

    fn wait_for_child(parent: u32, name: &str) -> u32 {
        let mut found = None;
        assert!(wait_until(Duration::from_secs(5), || {
            found = std::fs::read_to_string(format!("/proc/{parent}/task/{parent}/children"))
                .ok()
                .and_then(|text| {
                    text.split_whitespace()
                        .filter_map(|pid| pid.parse::<u32>().ok())
                        .find(|pid| {
                            std::fs::read_to_string(format!("/proc/{pid}/comm"))
                                .is_ok_and(|comm| comm.trim() == name)
                        })
                });
            found.is_some()
        }));
        found.unwrap()
    }

    #[test]
    #[ignore = "not tested: requires the system-mode container (CEDEGRID_TEST_SYSTEM_CONTAINER)"]
    fn purge_leaves_the_host_as_before_the_install() {
        assert!(enabled(), "{GATE}");
        install();
        purge();
        let before = inventory();
        install();
        // The install adds exactly the contract's items (contracts/units-and-slices.md).
        let added: Vec<String> = inventory()
            .into_iter()
            .filter(|item| !before.contains(item))
            .collect();
        let expected = [
            "group cedegrid",
            "path /etc/cedegrid",
            "path /run/cedegrid",
            "path /usr/bin/cedegrid",
            "path /usr/lib/sysusers.d/cedegrid-system.conf",
            "unit cedegrid-broker.service",
            "unit cedegrid-broker.socket",
            "unit cedegrid.slice",
            "unit cedegrid_idle.slice",
            "unit-file cedegrid-broker.service",
            "unit-file cedegrid-broker.socket",
            "unit-file cedegrid.slice",
            "unit-file cedegrid_idle.slice",
        ];
        for item in &added {
            assert!(
                expected.contains(&item.as_str()),
                "unexpected after install: {item}"
            );
        }
        for item in [
            "group cedegrid",
            "unit-file cedegrid-broker.socket",
            "path /usr/bin/cedegrid",
        ] {
            assert!(
                added.iter().any(|added| added == item),
                "missing after install: {item}"
            );
        }
        assert!(cedegrid_as(MEMBER, &["system", "start"]).status.success());
        let installed = inventory();
        assert!(
            installed.iter().any(|item| item == "group cedegrid"),
            "{installed:?}"
        );
        purge();
        assert!(
            wait_until(Duration::from_secs(10), || inventory() == before),
            "before {before:?}, after {:?}",
            inventory()
        );
        install();
    }

    /// The broker's CPU time, its reaped children's included, in clock ticks, and its RSS in kB.
    fn broker_cost() -> (u64, u64) {
        let pid = show("cedegrid-broker.service", "MainPID")["MainPID"].clone();
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        let fields: Vec<&str> = stat
            .rsplit_once(')')
            .unwrap()
            .1
            .split_whitespace()
            .collect();
        // utime, stime, cutime and cstime are fields 14 to 17; the first after ')' is field 3.
        let ticks: u64 = (14..=17)
            .map(|n| fields[n - 3].parse::<u64>().unwrap())
            .sum();
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
        let rss = status
            .lines()
            .find_map(|line| line.strip_prefix("VmRSS:"))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|kb| kb.parse().ok())
            .unwrap();
        (ticks, rss)
    }

    #[test]
    #[ignore = "not tested: requires the system-mode container (CEDEGRID_TEST_SYSTEM_CONTAINER)"]
    fn fifty_members_cost_the_broker_little() {
        assert!(enabled(), "{GATE}");
        install();
        let names: Vec<String> = (0..50).map(|n| format!("load{n:02}")).collect();
        for name in &names {
            if !run("id", &["-u", name]).status.success() {
                ok("useradd", &["-m", "-U", name]);
            }
            ok("usermod", &["-aG", "cedegrid", name]);
            let output = cedegrid_as(name, &["system", "start"]);
            assert!(
                stdout(&output).contains(r#""system":true"#),
                "{name}: {output:?}"
            );
        }
        // Let the starts settle and the OOM loop check each new unit's marker once (a one-time
        // cost per unit), then measure 60 s of the broker at rest.
        std::thread::sleep(Duration::from_secs(35));
        let (before, _) = broker_cost();
        let window = Duration::from_secs(60);
        std::thread::sleep(window);
        let (after, rss_kb) = broker_cost();
        // SAFETY: sysconf has no preconditions.
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
        let cpu_share = (after - before) as f64 / hz / window.as_secs_f64();
        for name in &names {
            stop_units(name);
        }
        eprintln!(
            "broker with 100 units: {:.4}% of one CPU, {rss_kb} kB RSS",
            cpu_share * 100.0
        );
        // Provisional bounds (SC-012): 0.1% of one CPU and 32 MiB, for the shipped (release)
        // build; a debug build only reports its figure.
        assert!(rss_kb < 32 * 1024, "{rss_kb} kB");
        if std::env::var("CEDEGRID_TEST_RELEASE").as_deref() == Ok("1") {
            assert!(cpu_share < 0.001, "{:.4}% of one CPU", cpu_share * 100.0);
        } else {
            eprintln!("CPU bound not checked: debug build; rerun with CEDEGRID_TEST_RELEASE=1");
        }
    }

    #[test]
    #[ignore = "not tested: requires the system-mode container (CEDEGRID_TEST_SYSTEM_CONTAINER)"]
    fn units_lay_out_an_agent_cgroup_and_a_jobs_cgroup() {
        assert!(enabled(), "{GATE}");
        install();
        stop_units(MEMBER);
        assert!(cedegrid_as(MEMBER, &["system", "start"]).status.success());
        for unit in [ordinary(MEMBER_UID), idle(MEMBER_UID)] {
            let group = show(&unit, "ControlGroup")["ControlGroup"].clone();
            let dir = format!("/sys/fs/cgroup{group}");
            assert!(
                wait_until(Duration::from_secs(10), || {
                    std::fs::read_to_string(format!("{dir}/agent/cgroup.procs"))
                        .is_ok_and(|procs| !procs.trim().is_empty())
                }),
                "{unit}: no agent process"
            );
            let own = std::fs::read_to_string(format!("{dir}/cgroup.procs")).unwrap();
            assert!(own.trim().is_empty(), "{unit} root holds {own}");
            let jobs = std::fs::read_to_string(format!("{dir}/jobs/cgroup.subtree_control"))
                .unwrap_or_default();
            for controller in ["cpu", "memory", "pids"] {
                assert!(
                    jobs.split_whitespace().any(|c| c == controller),
                    "{unit}: {jobs}"
                );
            }
        }
        stop_units(MEMBER);
    }

    #[test]
    #[ignore = "not tested: requires the system-mode container (CEDEGRID_TEST_SYSTEM_CONTAINER)"]
    fn without_a_configuration_the_unit_holds_and_says_why() {
        assert!(enabled(), "{GATE}");
        install();
        stop_units(MEMBER);
        assert!(cedegrid_as(MEMBER, &["system", "start"]).status.success());
        let unit = ordinary(MEMBER_UID);
        assert!(wait_until(Duration::from_secs(10), || {
            ok("journalctl", &["-u", &unit, "--no-pager", "-o", "cat"])
                .contains("no agent configuration")
        }));
        assert_eq!(show(&unit, "ActiveState")["ActiveState"], "active");
        stop_units(MEMBER);
    }

    #[test]
    #[ignore = "not tested: requires the system-mode container (CEDEGRID_TEST_SYSTEM_CONTAINER)"]
    fn the_confined_broker_moves_a_members_job() {
        assert!(enabled(), "{GATE}");
        install();
        stop_units(MEMBER);
        assert!(cedegrid_as(MEMBER, &["system", "start"]).status.success());
        let cgroup_of = |unit: &str| show(unit, "ControlGroup")["ControlGroup"].clone();
        let (ordinary_cg, idle_cg) = (
            cgroup_of(&ordinary(MEMBER_UID)),
            cgroup_of(&idle(MEMBER_UID)),
        );
        for dir in [&ordinary_cg, &idle_cg] {
            assert!(wait_until(Duration::from_secs(10), || {
                std::path::Path::new(&format!("/sys/fs/cgroup{dir}/jobs")).is_dir()
            }));
        }
        let source = format!("{ordinary_cg}/jobs/cedegrid-neutral-mv1");
        let target = format!("{idle_cg}/jobs/cedegrid-yield_first-mv1");
        for leaf in [&source, &target] {
            ok(
                "runuser",
                &[
                    "-u",
                    MEMBER,
                    "--",
                    "mkdir",
                    &format!("/sys/fs/cgroup{leaf}"),
                ],
            );
        }
        let mut child = Command::new("runuser")
            .args(["-u", MEMBER, "--", "sleep", "300"])
            .spawn()
            .unwrap();
        let pid = wait_for_child(child.id(), "sleep");
        // Test setup only: the job's process is placed in its leaf as the agent would start it.
        std::fs::write(
            format!("/sys/fs/cgroup{source}/cgroup.procs"),
            pid.to_string(),
        )
        .unwrap();
        let reply = raw_request(
            Some(MEMBER),
            b"{\"protocol\":1,\"kind\":\"move\",\"job\":\"mv1\",\"mode\":\"yield_first\"}\n",
            false,
        );
        let now = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap();
        let freeze = std::fs::read_to_string(format!("/sys/fs/cgroup{source}/cgroup.freeze"))
            .unwrap_or_default();
        child.kill().unwrap();
        let _ = child.wait();
        stop_units(MEMBER);
        assert_eq!(reply_code(&reply), "ok", "{reply}");
        assert_eq!(now.trim(), format!("0::{target}"));
        assert_eq!(freeze.trim(), "0");
        let journal = ok(
            "journalctl",
            &["-u", "cedegrid-broker.service", "--no-pager", "-o", "cat"],
        );
        assert!(journal.contains("move uid="), "no move line in the journal");
    }

    #[test]
    #[ignore = "not tested: requires the system-mode container (CEDEGRID_TEST_SYSTEM_CONTAINER)"]
    fn twenty_starts_all_land_in_the_members_units() {
        assert!(enabled(), "{GATE}");
        install();
        let unit = ordinary(MEMBER_UID);
        for round in 0..20 {
            stop_units(MEMBER);
            let output = cedegrid_as(MEMBER, &["system", "start"]);
            assert!(
                stdout(&output).contains(r#""system":true"#),
                "round {round}: {output:?}"
            );
            assert!(
                wait_until(Duration::from_secs(10), || {
                    show(&unit, "ActiveState")["ActiveState"] == "active"
                }),
                "round {round}"
            );
            assert_eq!(
                show(&unit, "User")["User"],
                MEMBER_UID.to_string(),
                "round {round}"
            );
        }
        stop_units(MEMBER);
    }

    #[test]
    #[ignore = "not tested: requires the system-mode container (CEDEGRID_TEST_SYSTEM_CONTAINER)"]
    fn the_idle_slice_has_cpu_idle() {
        assert!(enabled(), "{GATE}");
        install();
        assert!(cedegrid_as(MEMBER, &["system", "start"]).status.success());
        // Directly below the root, beside the other top-level groups.
        let group = show("cedegrid_idle.slice", "ControlGroup")["ControlGroup"].clone();
        let value = std::fs::read_to_string("/sys/fs/cgroup/cedegrid_idle.slice/cpu.idle")
            .unwrap_or_default();
        stop_units(MEMBER);
        assert_eq!(group, "/cedegrid_idle.slice");
        assert_eq!(value.trim(), "1", "cpu.idle on cedegrid_idle.slice");
    }
}

// T018, T023: the agent's side of the unit (leaf names and the jobs' delegated root).

mod agent_in_unit {
    use cedegrid::broker::ModeName;
    use cedegrid::broker::oom::parse_leaf;
    use cedegrid::cgroup::{leaf_label, system_unit_jobs_root};
    use cedegrid::contention::{ContentionMode, ResourceKind};
    use cedegrid::execution_model::{AllocationClass, JobContention, LaunchRequest};
    use std::path::{Path, PathBuf};

    fn request(task_id: &str, ram: Option<ContentionMode>) -> LaunchRequest {
        LaunchRequest {
            task_id: task_id.into(),
            assignment_id: "a".into(),
            argv: vec!["true".into()],
            cwd: PathBuf::from("/"),
            env: Default::default(),
            resources: Default::default(),
            replay_safe: true,
            class: AllocationClass::Opportunistic,
            no_escape: true,
            single_process: true,
            managed_child_limit: 0,
            max_attempts: None,
            input_artifacts: vec![],
            required_controls: vec![],
            allow_fallback: false,
            contention: ram.map(|mode| JobContention {
                modes: [(ResourceKind::Ram, mode)].into(),
                admit_immediately: None,
                unprotected: None,
            }),
        }
    }

    #[test]
    fn leaf_labels_carry_the_ram_mode_and_job_and_parse_back() {
        let label = leaf_label(&request("job-7", Some(ContentionMode::Performance))).unwrap();
        assert_eq!(label, "performance-job-7");
        let leaf = format!("cedegrid-{label}.0011223344556677");
        assert_eq!(parse_leaf(&leaf), Some((ModeName::Performance, "job-7")));
        assert_eq!(
            parse_leaf("cedegrid-yield_first-a-b-c"),
            Some((ModeName::YieldFirst, "a-b-c"))
        );
        // No mode, or a task ID that cannot be named, keeps the plain random leaf.
        assert_eq!(leaf_label(&request("job-7", None)), None);
        assert_eq!(
            leaf_label(&request("job/7", Some(ContentionMode::Neutral))),
            None
        );
        assert_eq!(
            leaf_label(&request("", Some(ContentionMode::Neutral))),
            None
        );
        assert_eq!(
            parse_leaf("cedegrid-00112233445566778899aabbccddeeff"),
            None
        );
    }

    fn owner(uid: u32) -> impl Fn(&Path) -> Option<u32> {
        move |_| Some(uid)
    }

    #[test]
    fn the_jobs_root_is_accepted_only_inside_the_users_own_unit() {
        let mount = Path::new("/sys/fs/cgroup");
        for cgroup in [
            "/user.slice/user-1000.slice/cedegrid-u1000.service",
            "/user.slice/user-1000.slice/cedegrid-u1000.service/agent",
            "/cedegrid.slice/cedegrid-u1000.service",
        ] {
            let root =
                system_unit_jobs_root(cgroup, 1000, Some("ordinary:1000"), mount, owner(1000))
                    .unwrap_or_else(|error| panic!("{cgroup}: {error:#}"));
            assert!(root.ends_with("cedegrid-u1000.service/jobs"), "{root:?}");
        }
        let refused = [
            // Another user's unit, the idle unit, a session, a nested path.
            (
                "/cedegrid.slice/cedegrid-u1001.service",
                "ordinary:1000",
                1000,
            ),
            (
                "/cedegrid_idle.slice/cedegrid-idle-u1000.service",
                "ordinary:1000",
                1000,
            ),
            (
                "/user.slice/user-1000.slice/session-3.scope",
                "ordinary:1000",
                1000,
            ),
            (
                "/cedegrid.slice/cedegrid-u1000.service/jobs/x",
                "ordinary:1000",
                1000,
            ),
            // Without the broker's marker, or not delegated to the user.
            ("/cedegrid.slice/cedegrid-u1000.service", "idle:1000", 1000),
            (
                "/cedegrid.slice/cedegrid-u1000.service",
                "ordinary:1001",
                1000,
            ),
            ("/cedegrid.slice/cedegrid-u1000.service", "ordinary:1000", 0),
        ];
        for (cgroup, marker, dir_owner) in refused {
            assert!(
                system_unit_jobs_root(cgroup, 1000, Some(marker), mount, owner(dir_owner)).is_err(),
                "{cgroup} {marker} {dir_owner}"
            );
        }
        assert!(
            system_unit_jobs_root(
                "/cedegrid.slice/cedegrid-u1000.service",
                1000,
                None,
                mount,
                owner(1000)
            )
            .is_err()
        );
    }
}

#[test]
fn the_unit_agent_reads_its_configuration_from_the_users_config_dir() {
    use cedegrid::system_mode::unit_config_dir;
    use std::ffi::OsStr;
    use std::path::PathBuf;
    assert_eq!(
        unit_config_dir(
            Some(OsStr::new("/home/test/.xdg")),
            Some(OsStr::new("/home/test"))
        ),
        Some(PathBuf::from("/home/test/.xdg/cedegrid"))
    );
    assert_eq!(
        unit_config_dir(None, Some(OsStr::new("/home/test"))),
        Some(PathBuf::from("/home/test/.config/cedegrid"))
    );
    assert_eq!(
        unit_config_dir(Some(OsStr::new("")), Some(OsStr::new("/home/test"))),
        Some(PathBuf::from("/home/test/.config/cedegrid"))
    );
    assert_eq!(unit_config_dir(None, None), None);
    assert_eq!(unit_config_dir(Some(OsStr::new("relative")), None), None);
}

#[test]
fn outside_a_unit_the_agent_keeps_its_own_oom_marks() {
    // The flag is set only by the agent's start inside a checked System mode unit.
    assert!(!cedegrid::system_mode::in_unit());
}

// T051, T054: the agent in a unit applies the administrator policy to each launch.

mod policy_in_unit {
    use cedegrid::broker::admin_policy::AdminPolicy;
    use cedegrid::contention::ContentionMode::{Neutral, Performance, YieldFirst};
    use cedegrid::system_mode::launch_refusal_with;

    fn policy(text: &str) -> anyhow::Result<AdminPolicy> {
        AdminPolicy::parse(text, 1000)
    }

    #[test]
    fn unprotected_needs_its_own_policy_key() {
        use cedegrid::system_mode::unprotected_refusal_with;
        // Absent or false: refused, naming the key; true: permitted.
        for text in ["", "[modes]\nunprotected = false\n"] {
            let (code, line) = unprotected_refusal_with(policy(text)).unwrap();
            assert_eq!(code, "unprotected_policy");
            assert!(
                line.starts_with("ERR_CEDEGRID_UNPROTECTED_POLICY:"),
                "{line}"
            );
            assert!(line.contains("modes.unprotected"), "{line}");
        }
        assert_eq!(
            unprotected_refusal_with(policy("[modes]\nunprotected = true\n")),
            None
        );
        // The allowed modes do not grant it, and an unreadable policy refuses.
        let all =
            "[modes]\nallowed = [\"yield_first\", \"neutral\", \"performance\", \"aggressive\"]\n";
        assert!(unprotected_refusal_with(policy(all)).is_some());
        let (code, _) = unprotected_refusal_with(policy("unknown = 1\n")).unwrap();
        assert_eq!(code, "policy_refused");
        let (code, _) =
            unprotected_refusal_with(policy("[modes]\nunprotected = \"yes\"\n")).unwrap();
        assert_eq!(code, "policy_refused");
        let entries = policy("").unwrap().entries();
        assert!(
            entries
                .iter()
                .any(|(key, value, _)| *key == "modes.unprotected" && value == "false")
        );
    }

    #[test]
    fn modes_outside_the_policy_are_refused_naming_the_key() {
        let only_neutral = || policy("[modes]\nallowed = [\"yield_first\", \"neutral\"]\n");
        assert_eq!(
            launch_refusal_with(only_neutral(), &[Neutral, YieldFirst], 0),
            None
        );
        let (code, detail) =
            launch_refusal_with(only_neutral(), &[Neutral, Performance], 0).unwrap();
        assert_eq!(code, "mode_not_allowed");
        assert!(detail.contains("modes.allowed"), "{detail}");
        assert!(detail.contains("performance"), "{detail}");
    }

    #[test]
    fn tasks_past_the_policy_limit_are_refused() {
        let two = || policy("[limits]\ntasks_per_user = 2\n");
        assert_eq!(launch_refusal_with(two(), &[Performance], 1), None);
        let (code, detail) = launch_refusal_with(two(), &[Performance], 2).unwrap();
        assert_eq!(code, "task_limit");
        assert!(detail.contains("limits.tasks_per_user"), "{detail}");
    }

    #[test]
    fn an_unreadable_policy_refuses_every_launch() {
        let (code, _) = launch_refusal_with(policy("unknown = 1\n"), &[Neutral], 0).unwrap();
        assert_eq!(code, "policy_refused");
    }
}

#[test]
fn a_jobs_modes_are_its_requests_capped_by_the_node() {
    use cedegrid::config::ContentionConfig;
    use cedegrid::contention::{ContentionMode, ResourceKind};
    use cedegrid::execution_model::JobContention;
    use cedegrid::system_mode::job_modes;
    let node = ContentionConfig {
        default_mode: ContentionMode::Neutral,
        ..ContentionConfig::default()
    };
    // No request: the node's mode everywhere.
    assert!(
        job_modes(&node, None)
            .iter()
            .all(|mode| *mode == ContentionMode::Neutral)
    );
    // A request above the node's most aggressive mode is capped there.
    let request = JobContention {
        modes: [
            (ResourceKind::Ram, ContentionMode::Performance),
            (ResourceKind::Cpu, ContentionMode::YieldFirst),
        ]
        .into(),
        admit_immediately: None,
        unprotected: None,
    };
    let modes = job_modes(&node, Some(&request));
    assert_eq!(modes[0], ContentionMode::YieldFirst);
    assert_eq!(modes[1], ContentionMode::Neutral);
}

#[test]
fn an_agent_in_a_unit_runs_at_the_full_level_on_its_jobs_cgroup() {
    use cedegrid::config::Config;
    use cedegrid::system_mode::{UNIT_CONTROLS, apply_unit};
    use std::path::PathBuf;
    let jobs = PathBuf::from("/sys/fs/cgroup/cedegrid.slice/cedegrid-u1500.service/jobs");
    // A default configuration: the cgroup backend off and nothing authorized.
    let mut config = Config::default();
    config.ram.prefer_own_oom_victim = false;
    assert!(apply_unit(&mut config, jobs.clone()).is_empty());
    assert!(config.cgroup.enabled);
    assert_eq!(config.cgroup.authorized_controls, UNIT_CONTROLS);
    assert_eq!(config.cgroup.delegated_root.as_ref(), Some(&jobs));
    assert_eq!(config.kernel.delegated_root.as_ref(), Some(&jobs));
    config.cgroup.validate().unwrap();
    // A user's own list is kept, with no control twice, and the OOM preference is turned off.
    let mut config = Config::default();
    config.cgroup.authorized_controls = vec!["cpu.weight".into(), "memory.max".into()];
    config.kernel.delegated_root = Some(PathBuf::from("/elsewhere"));
    config.ram.prefer_own_oom_victim = true;
    let warnings = apply_unit(&mut config, jobs.clone());
    assert_eq!(warnings.len(), 1);
    assert!(!config.ram.prefer_own_oom_victim);
    assert_eq!(config.kernel.delegated_root.as_ref(), Some(&jobs));
    let mut controls = config.cgroup.authorized_controls.clone();
    controls.sort();
    controls.dedup();
    assert_eq!(controls.len(), UNIT_CONTROLS.len() + 1);
    config.cgroup.validate().unwrap();
}

#[test]
fn a_removed_members_units_stop_once_no_job_runs() {
    use cedegrid::broker::units::removed_members_to_stop;
    // 1500 removed, idle; 1501 removed, a job in its ordinary unit; 1502 a member; 1503 unknown.
    let units = [
        (1500, false),
        (1500, false),
        (1501, true),
        (1501, false),
        (1502, false),
        (1503, false),
    ];
    let member = |uid: u32| match uid {
        1500 | 1501 => Some(false),
        1502 => Some(true),
        _ => None,
    };
    assert_eq!(
        removed_members_to_stop(&units, &member),
        [1500].into_iter().collect()
    );
    // Once the job ends, the second user's units stop too.
    let units = [(1501, false), (1501, false)];
    assert_eq!(
        removed_members_to_stop(&units, &member),
        [1501].into_iter().collect()
    );
}

#[test]
fn a_units_config_needs_no_delegated_root_of_its_own() {
    use cedegrid::config::{load_for_run, load_for_run_with};
    use cedegrid::system_mode::{UNIT_CONTROLS, unit_table};
    let dir = std::env::temp_dir().join(format!("cedegrid-unit-config-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("cedegrid.toml");
    // The backend on, as a member writes it, with no delegated root and one control of its own.
    std::fs::write(
        &path,
        "config_version = 1\nnode_id = \"n1\"\nstate_dir = \"state\"\n\n[cgroup]\nenabled = true\nauthorized_controls = [\"cgroup.procs\", \"memory.max\"]\n",
    )
    .unwrap();
    // Outside a unit the file is refused, as before.
    assert!(load_for_run(&path).is_err());
    let jobs = std::path::Path::new("/sys/fs/cgroup/cedegrid.slice/cedegrid-u1500.service/jobs");
    let (config, _) = load_for_run_with(&path, |table| unit_table(table, jobs)).unwrap();
    assert!(config.cgroup.enabled);
    assert_eq!(config.cgroup.delegated_root.as_deref(), Some(jobs));
    assert_eq!(config.kernel.delegated_root.as_deref(), Some(jobs));
    for control in UNIT_CONTROLS {
        assert!(
            config
                .cgroup
                .authorized_controls
                .iter()
                .any(|c| c == control)
        );
    }
    assert!(
        config
            .cgroup
            .authorized_controls
            .iter()
            .any(|c| c == "memory.max")
    );
    assert_eq!(
        config.state_dir,
        path.canonicalize().unwrap().parent().unwrap().join("state")
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_stop_waits_for_both_units_and_nothing_else_waits_longer_than_two_seconds() {
    use cedegrid::broker::systemd::SYSTEMD_TIMEOUT;
    use cedegrid::system_mode::{CLIENT_TIMEOUT, reply_timeout};
    // A stop of two units takes up to two systemd calls; every other request falls back fast.
    assert!(reply_timeout(&Request::Stop) > SYSTEMD_TIMEOUT * 2);
    for request in [Request::Status, Request::Start { mode: None }] {
        assert_eq!(reply_timeout(&request), CLIENT_TIMEOUT);
    }
    assert_eq!(CLIENT_TIMEOUT, Duration::from_secs(2));
}

/// 006 FR-016, FR-018: the raise is wanted while an unprotected run is unreleased and no other
/// account's Performance or Aggressive CedeGrid work arrived on the CPU before own work.
#[test]
fn the_raise_is_wanted_only_without_a_protected_first_comer() {
    use cedegrid::model::{CedeGridWork, PrincipalUsage};
    use cedegrid::system_mode::raise_wanted;
    let other = |arrival: Option<u64>, protected: Option<u64>| PrincipalUsage {
        uid: Some(1002),
        cedegrid_cpu: Some(CedeGridWork {
            arrival_unix_ms: arrival,
            protected_arrival_unix_ms: protected,
        }),
        ..PrincipalUsage::default()
    };
    let plain = PrincipalUsage {
        uid: Some(1003),
        cpu_millicores: 2000,
        ..PrincipalUsage::default()
    };
    assert!(!raise_wanted(0, Some(100), &[]));
    assert!(raise_wanted(1, Some(100), std::slice::from_ref(&plain)));
    // A protected first-comer, or a tie, holds the raise; a later one or one that only yields
    // does not.
    assert!(!raise_wanted(1, Some(100), &[other(Some(50), Some(50))]));
    assert!(!raise_wanted(1, Some(100), &[other(Some(100), Some(100))]));
    assert!(raise_wanted(1, Some(100), &[other(Some(50), None)]));
    assert!(raise_wanted(1, Some(100), &[other(Some(50), Some(150))]));
    // Without own arrival, any protected CedeGrid work holds it.
    assert!(!raise_wanted(1, None, &[other(Some(500), Some(500))]));
    assert!(raise_wanted(1, None, &[plain]));
}

/// 006 FR-026: the accounts that keep priority over plain users from holding are those with
/// CedeGrid work on the CPU whose account slice is below the weight. An unreadable slice or an
/// unknown owner counts as below; plain accounts and this one do not count.
#[test]
fn the_plain_raise_waits_for_cedegrid_accounts_below_the_weight() {
    use cedegrid::model::{CedeGridWork, PrincipalUsage};
    use cedegrid::system_mode::below_plain_weight;
    let cedegrid = |uid: Option<u32>| PrincipalUsage {
        uid,
        cedegrid_cpu: Some(CedeGridWork::default()),
        ..PrincipalUsage::default()
    };
    let principals = [
        PrincipalUsage {
            uid: Some(2000),
            is_owner: true,
            cedegrid_cpu: Some(CedeGridWork::default()),
            ..PrincipalUsage::default()
        },
        PrincipalUsage {
            uid: Some(2001),
            cpu_millicores: 4000,
            ..PrincipalUsage::default()
        },
        cedegrid(Some(2002)),
        cedegrid(Some(2003)),
        cedegrid(Some(2004)),
        cedegrid(Some(2005)),
    ];
    let weights = |uid: u32| match uid {
        2002 => Some(200),
        2003 => Some(100),
        2005 => Some(400),
        _ => None,
    };
    assert_eq!(
        below_plain_weight(200, &principals, weights),
        [Some(2003), Some(2004)].into()
    );
    assert!(below_plain_weight(200, &principals[..3], weights).is_empty());
    assert_eq!(
        below_plain_weight(200, &[cedegrid(None)], weights),
        [None].into()
    );
}

/// 006 FR-026: the raise is wanted while no CedeGrid account is below the weight. One falling
/// below a held raise has the two-interval withdrawal window to follow; without a held raise,
/// or past the window, it is not wanted until none is below. An unknown owner ends it at once.
#[test]
fn the_plain_raise_holds_only_while_no_cedegrid_account_is_below() {
    use cedegrid::system_mode::PlainSteer;
    use std::collections::BTreeSet;
    let below = |uids: &[u32]| -> BTreeSet<Option<u32>> { uids.iter().map(|u| Some(*u)).collect() };
    let mut steer = PlainSteer::default();
    assert!(!steer.wanted(false, &below(&[]), 0, 2000));
    // Not holding: another account below keeps it off.
    assert!(!steer.wanted(true, &below(&[2002]), 2000, 2000));
    assert!(!steer.wanted(true, &below(&[2002]), 9000, 2000));
    // None below: held. One falls below: kept for the window, then not.
    assert!(steer.wanted(true, &below(&[]), 10_000, 2000));
    assert!(steer.wanted(true, &below(&[2002]), 11_000, 2000));
    assert!(steer.wanted(true, &below(&[2002, 2003]), 12_999, 2000));
    assert!(!steer.wanted(true, &below(&[2002]), 13_000, 2000));
    assert!(!steer.wanted(true, &below(&[2002]), 14_000, 2000));
    // It followed: held again, and a later fall gets a new window.
    assert!(steer.wanted(true, &below(&[]), 20_000, 2000));
    assert!(steer.wanted(true, &below(&[2003]), 21_000, 2000));
    assert!(steer.wanted(true, &below(&[]), 22_000, 2000));
    assert!(steer.wanted(true, &below(&[2003]), 30_000, 2000));
    // Not eligible, or an unknown owner, ends it at once.
    assert!(!steer.wanted(false, &below(&[]), 31_000, 2000));
    assert!(steer.wanted(true, &below(&[]), 32_000, 2000));
    assert!(!steer.wanted(true, &[None].into(), 32_500, 2000));
    assert!(!steer.wanted(true, &below(&[2002]), 33_000, 2000));
}

/// 006 FR-029: doctor and observe show whether priority over plain users is chosen, permitted
/// and in force, with the weight, and the reason when it is not.
#[test]
fn priority_over_plain_users_shows_chosen_permitted_and_in_force() {
    use cedegrid::system_mode::{PlainPriorityHost, plain_priority_status, plain_priority_text};
    let status = |chosen, host: PlainPriorityHost, own: Option<u64>, below: usize| {
        plain_priority_status(chosen, &host, own, below)
    };
    let held = status(true, PlainPriorityHost::Permitted(200), Some(200), 0);
    assert!(held.chosen && held.permitted && held.in_force);
    assert_eq!(held.weight, Some(200));
    assert_eq!(held.reason, None);
    assert_eq!(
        plain_priority_text(&held),
        "\nPriority over plain users\n  chosen      yes\n  permitted   yes, at weight 200\n  in force    yes\n"
    );
    let cases = [
        (
            status(false, PlainPriorityHost::Permitted(200), Some(100), 0),
            "not chosen; run cedegrid setup --priority-over-plain to choose it",
        ),
        (
            status(true, PlainPriorityHost::Rootless, None, 0),
            "this host runs CedeGrid rootless; it needs System mode",
        ),
        (
            status(true, PlainPriorityHost::NotPermitted, Some(100), 0),
            "the System mode policy does not permit it (modes.plain_priority)",
        ),
        (
            status(
                true,
                PlainPriorityHost::Unreadable("bad".into()),
                Some(100),
                0,
            ),
            "the System mode policy cannot be read: bad",
        ),
        (
            status(true, PlainPriorityHost::Permitted(200), Some(100), 1),
            "another account's CedeGrid work on the CPU runs below weight 200",
        ),
        (
            status(true, PlainPriorityHost::Permitted(200), Some(100), 0),
            "it holds only while own Performance or Aggressive CPU work runs",
        ),
        (
            status(true, PlainPriorityHost::Permitted(200), Some(400), 0),
            "the unprotected raise is in force at weight 400",
        ),
    ];
    for (status, reason) in cases {
        assert!(!status.in_force, "{reason}");
        assert_eq!(status.reason.as_deref(), Some(reason));
    }
    let text = plain_priority_text(&status(true, PlainPriorityHost::NotPermitted, None, 0));
    assert!(text.contains("  permitted   no\n"), "{text}");
    assert!(
        text.contains("  in force    no: the System mode policy does not permit it"),
        "{text}"
    );
}

/// 006 FR-026: where both raises are wanted, the one with the higher policy weight is asked
/// for, unprotected on a tie.
#[test]
fn the_higher_raise_wins() {
    use cedegrid::system_mode::pick_raise;
    assert_eq!(pick_raise(false, false, 400, 200), None);
    assert_eq!(
        pick_raise(true, false, 400, 200),
        Some(RaiseLevel::Unprotected)
    );
    assert_eq!(pick_raise(false, true, 400, 200), Some(RaiseLevel::Plain));
    assert_eq!(
        pick_raise(true, true, 400, 200),
        Some(RaiseLevel::Unprotected)
    );
    assert_eq!(pick_raise(true, true, 150, 200), Some(RaiseLevel::Plain));
    assert_eq!(
        pick_raise(true, true, 200, 200),
        Some(RaiseLevel::Unprotected)
    );
}

/// 006 FR-026: an account slice's CPU weight is read from the cgroup tree; a missing or
/// malformed file reads as unknown.
#[test]
fn an_account_slice_weight_is_read_from_the_cgroup_tree() {
    use cedegrid::broker::units::account_slice_weight_in;
    let root = tempfile::tempdir().unwrap();
    let slice = root.path().join("user.slice/user-2002.slice");
    std::fs::create_dir_all(&slice).unwrap();
    std::fs::write(slice.join("cpu.weight"), "200\n").unwrap();
    assert_eq!(account_slice_weight_in(root.path(), 2002), Some(200));
    assert_eq!(account_slice_weight_in(root.path(), 2003), None);
    std::fs::write(slice.join("cpu.weight"), "max\n").unwrap();
    assert_eq!(account_slice_weight_in(root.path(), 2002), None);
}

#[test]
fn an_unprotected_grant_starts_only_with_every_condition_met() {
    use cedegrid::contention::{ContentionMode, ResourceKind};
    use cedegrid::execution_model::{JobContention, RunGrant};
    use cedegrid::state::Allowance;
    use cedegrid::system_mode::grant_refusal;
    // Unprotected applies to Performance as well as Aggressive work: no mode is a condition.
    let grant = |confirmed| JobContention {
        modes: [(ResourceKind::Cpu, ContentionMode::Performance)].into(),
        admit_immediately: None,
        unprotected: Some(RunGrant {
            confirmed,
            requested_unix_ms: 1,
        }),
    };
    let on = || Allowance::On { set_at_unix_ms: 1 };
    let off = || Allowance::Off { reason: None };
    let unreadable = || Allowance::Off {
        reason: Some("the allowance record cannot be read: corrupt".into()),
    };
    let permitted = || None;
    // Every condition met: it starts.
    assert_eq!(grant_refusal(Some(&grant(true)), on, permitted), None);
    // No grant: nothing to check.
    assert_eq!(grant_refusal(None, off, permitted), None);
    // Each missing condition is named, all of them in one line.
    let (code, line) = grant_refusal(Some(&grant(false)), off, || {
        Some((
            "unprotected_policy",
            "ERR_CEDEGRID_UNPROTECTED_POLICY: no".to_owned(),
        ))
    })
    .unwrap();
    assert_eq!(code, "unprotected_refused");
    for needle in [
        "ERR_CEDEGRID_UNPROTECTED_CONFIRMATION",
        "ERR_CEDEGRID_UNPROTECTED_NOT_ALLOWED",
        "ERR_CEDEGRID_UNPROTECTED_POLICY",
    ] {
        assert!(line.contains(needle), "{line}");
    }
    assert_eq!(line.lines().count(), 1);
    let (_, line) = grant_refusal(Some(&grant(true)), unreadable, permitted).unwrap();
    assert!(!line.contains("CAPPED"), "{line}");
    assert!(
        line.contains("ERR_CEDEGRID_UNPROTECTED_ALLOWANCE_UNREADABLE"),
        "{line}"
    );
    assert!(
        !line.contains("ERR_CEDEGRID_UNPROTECTED_NOT_ALLOWED"),
        "{line}"
    );
}
