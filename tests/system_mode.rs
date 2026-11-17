//! Feature 005, System mode: the broker's pure parts.
//!
//! Request parsing (T006), the administrator policy (T007), the unit builder (T008) and the score
//! function (T042). The container fixtures that need root and systemd come later and are opt-in.

use cedegrid::broker::admin_policy::{self, AdminPolicy, Source};
use cedegrid::broker::request::{
    JobId, MAX_REQUEST_BYTES, RefusalCode, Reply, Request, parse_frame,
};
use cedegrid::broker::score::{
    Lowering, OverShareMarking, ProcessKey, ScoreInput, ScoreWrite, ScoredProcess, ScoredTask,
    UNKNOWN_WINDOW, UserMemory, score_writes,
};
use cedegrid::broker::units::{
    self, Caller, OrdinarySlice, PamSession, Placement, UnitKind, systemd_run_args,
};
use cedegrid::broker::{INSTALLED_BINARY, ModeName, PROTOCOL};
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

#[test]
fn policy_file_values_and_sources() {
    let policy = AdminPolicy::parse(
        r#"
policy_version = 1

[modes]
allowed = ["yield_first", "neutral", "aggressive"]

[limits]
tasks_per_user = 8
tasks_max = 512
open_files = 65536
processes = 0
locked_memory_mib = 64

[principals]
uid_min = 2000
principal_min_percent = 5

[units]
pam_session = "cedegrid"
"#,
        1000,
    )
    .unwrap();
    assert!(policy.allows(ModeName::Aggressive));
    assert!(!policy.allows(ModeName::Performance));
    assert_eq!(policy.tasks_per_user.value, 8);
    assert_eq!(policy.tasks_max.value, 512);
    assert_eq!(policy.open_files.value, Some(65536));
    // 0 keeps systemd's default, and still counts as set in the file.
    assert_eq!(policy.processes.value, None);
    assert_eq!(policy.processes.source, Source::File);
    assert_eq!(policy.locked_memory_mib.value, Some(64));
    assert_eq!(policy.uid_min.value, 2000);
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
            "cedegrid-idle.slice"
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
    fn proc_text_parsers() {
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
        calls: Mutex<Vec<String>>,
        fail_start: bool,
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
                .push(format!("start {}", spec.name));
            if self.fail_start {
                return Err(Refusal::new(RefusalCode::SystemdFailed, "failed."));
            }
            self.put(&spec.name, ours(spec.kind, spec.uid, "active"));
            Ok(())
        }

        fn stop(&self, unit: &str) -> Result<(), Refusal> {
            self.calls.lock().unwrap().push(format!("stop {unit}"));
            self.units.lock().unwrap().remove(unit);
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
            .position(|call| call == "start cedegrid-u1234.service")
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

    fn state(systemd: FakeSystemd, members: &[u32], policy: Option<&'static str>) -> BrokerState {
        BrokerState {
            systemd: Box::new(systemd),
            accounts: Box::new(accounts(members)),
            load_policy: Box::new(move || match policy {
                Some(text) => AdminPolicy::parse(text, 1000),
                None => AdminPolicy::defaults(1000),
            }),
            host_uid_min: 1000,
            placement: PLACE,
            rate: Mutex::new(RateLimiter::new(8, 2)),
        }
    }

    fn line(json: &str) -> Vec<u8> {
        format!("{json}\n").into_bytes()
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
    fn moves_are_refused_until_built() {
        let state = state(FakeSystemd::default(), &[1234], None);
        let handled = handle(
            &state,
            peer(1234),
            &line(r#"{"protocol":1,"kind":"move","job":"j","mode":"neutral"}"#),
        );
        assert_eq!(handled.reply.code, Some(RefusalCode::MoveRefused));
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
