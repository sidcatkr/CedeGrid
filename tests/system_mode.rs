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
