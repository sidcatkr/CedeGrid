use cedegrid::{
    config::{self, Config, RuntimeClientConfig, RuntimeConfigKind},
    config_migration::{self, LegacyClientSemantics, MigrationOptions},
};
use serde_json::{Value, json};
use std::{fs, path::Path};

fn contract() -> Value {
    serde_json::from_str(include_str!("contracts/cedegrid-0.2.json")).unwrap()
}

fn legacy_client() -> String {
    json!({
        "endpoint": "https://example.test/",
        "tls": {"ca_cert":"tls/ca.pem", "certificate":"tls/client.pem", "private_key":"tls/client.key"}
    }).to_string()
}

fn options(root: &Path, kind: RuntimeConfigKind) -> MigrationOptions {
    MigrationOptions {
        kind,
        input: root.join("old/input.yaml"),
        output: root.join("new/配置/output.toml"),
        legacy_client_semantics: None,
        legacy_cwd: None,
    }
}

fn stage(options: &MigrationOptions, contents: &str) {
    fs::create_dir_all(options.input.parent().unwrap()).unwrap();
    fs::create_dir_all(options.output.parent().unwrap()).unwrap();
    fs::write(&options.input, contents).unwrap();
}

#[test]
fn shared_node_toml_contracts_execute() {
    let root = tempfile::tempdir().unwrap();
    let location = root.path().canonicalize().unwrap().join("node.toml");
    let mut executed = 0;
    for case in contract()["config_cases"].as_array().unwrap() {
        if case["kind"] != "node" || case.get("toml").is_none() || case.get("config_path").is_some()
        {
            continue;
        }
        executed += 1;
        let result = config::parse_runtime::<Config>(
            case["toml"].as_str().unwrap(),
            &location,
            RuntimeConfigKind::Node,
        );
        if case.get("error").is_some() {
            assert!(
                format!("{:#}", result.unwrap_err()).contains("ERR_CEDEGRID_CONFIG"),
                "{}",
                case["id"]
            );
        } else {
            let result = result.unwrap();
            let expected = &case["expected"]["cpu_weight"];
            if expected["kind"] == "none" {
                assert_eq!(result.cgroup.cpu_weight, None, "{}", case["id"]);
            } else {
                assert_eq!(
                    result.cgroup.cpu_weight.unwrap().to_string(),
                    expected["decimal"].as_str().unwrap(),
                    "{}",
                    case["id"]
                );
            }
            assert!(!result.state_dir.exists());
        }
    }
    assert_eq!(executed, 16);
}

#[test]
fn shared_client_scalar_contracts_preserve_integer_tokens_before_validation() {
    let root = tempfile::tempdir().unwrap();
    let location = root.path().canonicalize().unwrap().join("client.toml");
    let mut executed = 0;
    for case in contract()["config_cases"].as_array().unwrap() {
        if case["kind"] != "client_scalar" {
            continue;
        }
        executed += 1;
        let document = format!(
            "config_version = 1\nendpoint = 'https://example.test'\n{} = {}\n[tls]\nca_cert='tls/ca.pem'\ncertificate='tls/client.pem'\nprivate_key='tls/client.key'\n",
            case["field"].as_str().unwrap(),
            case["toml_value"].as_str().unwrap()
        );
        let result = config::parse_runtime::<RuntimeClientConfig>(
            &document,
            &location,
            RuntimeConfigKind::Client,
        );
        if case.get("error").is_some() {
            assert!(
                format!("{:#}", result.unwrap_err()).contains("ERR_CEDEGRID_CONFIG"),
                "{}",
                case["id"]
            );
        } else {
            assert_eq!(
                result.unwrap().max_transfer_bytes_per_second.to_string(),
                case["expected"]["decimal"].as_str().unwrap()
            );
        }
    }
    assert_eq!(executed, 6);
}

#[test]
fn shared_https_origins_execute_including_empty_components() {
    for case in contract()["origin_cases"].as_array().unwrap() {
        let result = config::normalize_https_origin(case["input"].as_str().unwrap());
        if case["accepted"] == true {
            assert_eq!(
                format!("{}/v1/rpc", result.unwrap()),
                case["rpc_url"].as_str().unwrap()
            );
        } else {
            assert!(result.is_err(), "{}", case["input"]);
        }
    }
    for input in [
        "https://example.test:",
        "https://[::1]:",
        "https://example.test\\other",
        " https://example.test",
        "https://example.test:443:1",
    ] {
        assert!(config::normalize_https_origin(input).is_err(), "{input}");
    }
}

#[test]
fn shared_client_origin_migration_contracts_execute() {
    for case in contract()["config_cases"].as_array().unwrap() {
        if case["kind"] != "client" {
            continue;
        }
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let mut options = options(&root, RuntimeConfigKind::Client);
        if let Some(path) = case["legacy_path"].as_str() {
            options.input = root.join(path);
        }
        if let Some(path) = case["output_path"].as_str() {
            options.output = root.join(path);
        }
        options.legacy_client_semantics = match case["options"]["legacy_client_semantics"].as_str()
        {
            Some("rust") => Some(LegacyClientSemantics::Rust),
            Some("python") => Some(LegacyClientSemantics::Python),
            _ => None,
        };
        if let Some(path) = case["options"]["legacy_cwd_relative_to_fixture_root"].as_str() {
            options.legacy_cwd = Some(root.join(path));
        }
        let contents = case["legacy_text"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(legacy_client);
        stage(&options, &contents);
        let result = config_migration::migrate(&options);
        assert_eq!(fs::read_to_string(&options.input).unwrap(), contents);
        if case.get("error").is_some() {
            assert!(
                format!("{:#}", result.unwrap_err()).contains("ERR_CEDEGRID_CONFIG"),
                "{}",
                case["id"]
            );
            assert!(!options.output.exists());
        } else {
            assert!(result.unwrap().effective_settings_preserved);
            let migrated: RuntimeClientConfig =
                config::load_runtime(&options.output, RuntimeConfigKind::Client).unwrap();
            assert_eq!(
                migrated.tls.ca_cert,
                root.join(
                    case["expected"]["tls_path_relative_to_fixture_root"]
                        .as_str()
                        .unwrap()
                )
            );
            assert!(
                migrated.tls.certificate.is_absolute() && migrated.tls.private_key.is_absolute()
            );
            if options.legacy_client_semantics == Some(LegacyClientSemantics::Rust) {
                assert_eq!(migrated.timeout_seconds, 10.0);
                assert_eq!(migrated.max_transfer_bytes_per_second, 0);
            } else {
                assert_eq!(migrated.timeout_seconds, 15.0);
                assert_eq!(migrated.max_transfer_bytes_per_second, 10 * 1024 * 1024);
            }
        }
    }
}

#[test]
fn moved_legacy_default_state_and_disabled_cpu_weight_survive_conversion() {
    let fixtures = contract();
    let case = fixtures["config_cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["id"] == "CFG-04-moved-legacy-default")
        .unwrap();
    for weight in [
        "",
        "cgroup:\n  cpu_weight: null\n",
        "cgroup:\n  cpu_weight: 100\n",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let mut options = options(&root, RuntimeConfigKind::Node);
        options.input = root.join(case["legacy_path"].as_str().unwrap());
        options.output = root.join(case["output_path"].as_str().unwrap());
        let source = format!("{}{weight}", case["legacy_text"].as_str().unwrap());
        stage(&options, &source);
        config_migration::migrate(&options).unwrap();
        let converted = Config::load(&options.output).unwrap();
        assert_eq!(
            converted.state_dir,
            root.join(
                case["expected"]["state_path_relative_to_fixture_root"]
                    .as_str()
                    .unwrap()
            )
        );
        assert!(!converted.state_dir.exists());
        assert_eq!(fs::read_to_string(&options.input).unwrap(), source);
        assert_eq!(
            converted.cgroup.cpu_weight,
            match weight {
                "" => Some(10),
                "cgroup:\n  cpu_weight: null\n" => None,
                _ => Some(100),
            }
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&options.output).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn canonical_config_symlink_and_missing_state_contract_executes() {
    let fixtures = contract();
    let case = fixtures["config_cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["id"] == "CFG-03-symlink-config")
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let target = root.join(case["canonical_config_path"].as_str().unwrap());
    let link = root.join(case["config_path"].as_str().unwrap());
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::create_dir_all(link.parent().unwrap()).unwrap();
    fs::write(&target, case["toml"].as_str().unwrap()).unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let config = Config::load(&link).unwrap();
    assert_eq!(
        config.state_dir,
        root.join(
            case["expected"]["state_path_relative_to_fixture_root"]
                .as_str()
                .unwrap()
        )
    );
    assert!(!config.state_dir.exists());
}

#[cfg(unix)]
#[test]
fn migration_uses_canonical_input_location_and_resolves_parent_after_symlink() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    fs::create_dir_all(root.join("real/配置")).unwrap();
    fs::create_dir_all(root.join("target/child")).unwrap();
    fs::create_dir_all(root.join("new")).unwrap();
    std::os::unix::fs::symlink(root.join("target/child"), root.join("real/配置/alias")).unwrap();
    let original = root.join("real/配置/node.yaml");
    fs::write(
        &original,
        "state_dir: alias/../future state\nmonitor:\n  interval_ms: 123\n",
    )
    .unwrap();
    let link = root.join("source.yaml");
    std::os::unix::fs::symlink(&original, &link).unwrap();
    let options = MigrationOptions {
        kind: RuntimeConfigKind::Node,
        input: link,
        output: root.join("new/node.toml"),
        legacy_client_semantics: None,
        legacy_cwd: None,
    };
    config_migration::migrate(&options).unwrap();
    let migrated = Config::load(&options.output).unwrap();
    assert_eq!(migrated.state_dir, root.join("target/future state"));
    assert_eq!(migrated.monitor.interval_ms, 123);
    assert_eq!(migrated.kernel.monitor_interval_ms, 123);
    assert!(!migrated.state_dir.exists());
}

#[test]
fn paths_do_not_expand_tilde_or_environment_and_versions_are_integer_only() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let node: Config = config::parse_runtime(
        "config_version=1\nstate_dir='~/${HOME}/future'\n",
        &root.join("node.toml"),
        RuntimeConfigKind::Node,
    )
    .unwrap();
    assert_eq!(node.state_dir, root.join("~/${HOME}/future"));
    for version in ["true", "1.0", "1979-05-27", "-1"] {
        assert!(
            config::parse_runtime::<Config>(
                &format!("config_version={version}\n"),
                &root.join("node.toml"),
                RuntimeConfigKind::Node
            )
            .is_err()
        );
    }
    let client = config::example(RuntimeConfigKind::Client).unwrap();
    let client: RuntimeClientConfig = config::parse_runtime(
        &client,
        &root.join("client.toml"),
        RuntimeConfigKind::Client,
    )
    .unwrap();
    assert_eq!(client.timeout_seconds, 15.0);
    assert_eq!(client.max_transfer_bytes_per_second, 10 * 1024 * 1024);
}

#[test]
fn client_only_zero_rate_and_strict_nested_role_settings_are_validated() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let client = config::example(RuntimeConfigKind::Client).unwrap().replace(
        "max_transfer_bytes_per_second = 10485760",
        "max_transfer_bytes_per_second = 0",
    );
    let settings: RuntimeClientConfig = config::parse_runtime(
        &client,
        &root.join("client.toml"),
        RuntimeConfigKind::Client,
    )
    .unwrap();
    assert_eq!(settings.max_transfer_bytes_per_second, 0);
    let agent = config::example(RuntimeConfigKind::Agent).unwrap().replace(
        "max_transfer_bytes_per_second = 10485760",
        "max_transfer_bytes_per_second = 0",
    );
    assert!(
        config::parse_runtime::<Value>(&agent, &root.join("agent.toml"), RuntimeConfigKind::Agent)
            .is_err()
    );
    let coordinator = config::example(RuntimeConfigKind::Coordinator)
        .unwrap()
        .replace(
            "role = \"operator\"",
            "role = \"operator\"\nnode_id = 'unexpected'",
        );
    assert!(
        config::parse_runtime::<Value>(
            &coordinator,
            &root.join("coordinator.toml"),
            RuntimeConfigKind::Coordinator
        )
        .is_err()
    );
}

#[test]
fn runtime_loaders_reject_legacy_formats_and_all_examples_are_semantically_valid() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    for kind in [
        RuntimeConfigKind::Node,
        RuntimeConfigKind::Coordinator,
        RuntimeConfigKind::Agent,
        RuntimeConfigKind::Client,
    ] {
        let text = config::example(kind).unwrap();
        let parsed: Value =
            config::parse_runtime(&text, &root.join("settings.toml"), kind).unwrap();
        assert!(parsed.is_object());
        let bad = text.replacen("config_version = 1", "config_version = 1\nextra = true", 1);
        assert!(config::parse_runtime::<Value>(&bad, &root.join("settings.toml"), kind).is_err());
        let bad = text.replacen("config_version = 1", "", 1);
        assert!(config::parse_runtime::<Value>(&bad, &root.join("settings.toml"), kind).is_err());
    }
    let path = root.join("node.yaml");
    for legacy in [
        "schema_version: 2\nnode_id: legacy\n",
        r#"{"config_version":1,"node_id":"legacy"}"#,
    ] {
        fs::write(&path, legacy).unwrap();
        assert!(Config::load(&path).is_err());
    }
}

#[test]
fn legacy_agent_and_coordinator_conversion_preserve_defaults_and_absolute_paths() {
    for kind in [RuntimeConfigKind::Agent, RuntimeConfigKind::Coordinator] {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let options = options(&root, kind);
        let mut source = json!({"tls":{"ca_cert":"tls/ca.pem","certificate":"tls/self.pem","private_key":"tls/self.key"}});
        if kind == RuntimeConfigKind::Agent {
            source["coordinator_url"] = json!("https://example.test/");
            source["capacity"] = json!({"cpu_millicores":1000,"ram_mib":256});
        } else {
            source["state_dir"] = json!("future state");
            source["listen"] = json!("127.0.0.1:7443");
            source["clients"] = json!({"0000000000000000000000000000000000000000000000000000000000000000":{"role":"operator"}});
        }
        stage(&options, &source.to_string());
        config_migration::migrate(&options).unwrap();
        let converted: Value = config::load_runtime(&options.output, kind).unwrap();
        assert_eq!(
            Path::new(converted["tls"]["ca_cert"].as_str().unwrap()),
            root.join("old/tls/ca.pem")
        );
        if kind == RuntimeConfigKind::Agent {
            assert_eq!(converted["max_workers"], 4);
            assert_eq!(converted["max_spool_bytes"], 2_u64 * 1024 * 1024 * 1024);
        } else {
            assert_eq!(
                Path::new(converted["state_dir"].as_str().unwrap()),
                root.join("old/future state")
            );
            assert_eq!(converted["lease_ms"], 10_000);
        }
    }
}

#[test]
fn duplicate_legacy_keys_unknown_fields_and_unrepresentable_integers_create_no_output() {
    for contents in [
        "schema_version: 2\nnode_id: first\nnode_id: second\n",
        r#"{"schema_version":2,"node_id":"first","node_id":"second"}"#,
        "schema_version: 2\nunrecognized: true\n",
        "schema_version: 3\n",
        "schema_version: 2\nram:\n  reserve_mib: 18446744073709551615\n",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let options = options(directory.path(), RuntimeConfigKind::Node);
        stage(&options, contents);
        assert!(config_migration::migrate(&options).is_err(), "{contents}");
        assert!(!options.output.exists());
        assert_eq!(fs::read_to_string(&options.input).unwrap(), contents);
        assert_eq!(
            fs::read_dir(options.output.parent().unwrap())
                .unwrap()
                .count(),
            0
        );
    }
}

#[test]
fn concurrent_converters_have_exactly_one_winner_without_overwriting() {
    let directory = tempfile::tempdir().unwrap();
    let mut left = options(directory.path(), RuntimeConfigKind::Node);
    let mut right = left.clone();
    left.input.set_file_name("left.yaml");
    right.input.set_file_name("right.yaml");
    stage(&left, "node_id: left\n");
    stage(&right, "node_id: right\n");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let outcomes = std::thread::scope(|scope| {
        let a = barrier.clone();
        let left = &left;
        let first = scope.spawn(move || {
            a.wait();
            config_migration::migrate(left)
        });
        let second = scope.spawn(move || {
            barrier.wait();
            config_migration::migrate(&right)
        });
        [first.join().unwrap(), second.join().unwrap()]
    });
    assert_eq!(outcomes.iter().filter(|result| result.is_ok()).count(), 1);
    let winner = outcomes
        .iter()
        .find_map(|result| result.as_ref().ok())
        .unwrap();
    let config = Config::load(&left.output).unwrap();
    assert_eq!(
        config.node_id,
        winner.input.file_stem().unwrap().to_str().unwrap()
    );
    assert_eq!(fs::read_to_string(&left.input).unwrap(), "node_id: left\n");
    assert_eq!(
        fs::read_dir(left.output.parent().unwrap()).unwrap().count(),
        1
    );
}

#[cfg(unix)]
#[test]
fn converter_refuses_existing_and_dangling_output_symlinks() {
    for dangling in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let options = options(directory.path(), RuntimeConfigKind::Node);
        stage(&options, "node_id: source\n");
        let other = directory.path().join("unrelated");
        if !dangling {
            fs::write(&other, b"do not replace").unwrap();
        }
        std::os::unix::fs::symlink(&other, &options.output).unwrap();
        assert!(config_migration::migrate(&options).is_err());
        assert!(
            fs::symlink_metadata(&options.output)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        if !dangling {
            assert_eq!(fs::read(&other).unwrap(), b"do not replace");
        }
    }
}

fn node_location() -> (tempfile::TempDir, std::path::PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let location = root.path().canonicalize().unwrap().join("node.toml");
    (root, location)
}

fn parse_node(body: &str) -> anyhow::Result<Config> {
    let (_root, location) = node_location();
    config::parse_runtime::<Config>(
        &format!("config_version = 1\n{body}\n"),
        &location,
        RuntimeConfigKind::Node,
    )
}

#[test]
fn contention_defaults_and_overrides_parse() {
    use cedegrid::config::{FloorPolicy, NeutralMemoryPolicy};
    use cedegrid::contention::{ContentionMode, ResourceKind};
    let defaults = Config::default().contention;
    assert_eq!(defaults.default_mode, ContentionMode::Neutral);
    // Without a cap, jobs reach no further than the node's own most aggressive mode.
    assert_eq!(defaults.max_mode, None);
    assert_eq!(defaults.max_mode(), ContentionMode::Neutral);
    assert_eq!(defaults.floor_policy, FloorPolicy::A);
    assert_eq!(defaults.neutral_memory_policy, NeutralMemoryPolicy::A);
    assert_eq!(defaults.thresholds(ResourceKind::Cpu).reclaim_s, 5);
    assert_eq!(defaults.guaranteed_hint_deadline_s, 5);
    let cpu = defaults.thresholds(ResourceKind::Cpu);
    assert_eq!(
        (
            cpu.yield_first_margin_percent,
            cpu.neutral_margin_percent,
            cpu.performance_margin_percent
        ),
        (15, 5, 2)
    );
    let compute = defaults.thresholds(ResourceKind::GpuCompute);
    assert_eq!(
        (
            compute.yield_first_margin_percent,
            compute.neutral_margin_percent,
            compute.performance_margin_percent
        ),
        (15, 5, 2)
    );
    for kind in [ResourceKind::Ram, ResourceKind::GpuMemory] {
        assert_eq!(defaults.thresholds(kind).yield_first_margin_percent, 20);
    }
    // No CPU reserve stacks on the margin by default.
    assert_eq!(Config::default().cpu.reserve_physical_cores, 0);
    assert_eq!(
        defaults
            .thresholds(ResourceKind::Ram)
            .guaranteed_breach_window_s,
        10
    );
    assert_eq!(
        defaults
            .thresholds(ResourceKind::GpuMemory)
            .noise_tolerance_mib,
        256
    );
    let parsed = parse_node(
        "[contention]\ndefault_mode = \"yield_first\"\nacknowledge_caution = [\"performance_mode\", \"relaxed_detection\"]\n[contention.gpu_memory]\nnoise_tolerance_mib = 512\n[contention.cpu]\nmode = \"performance\"",
    )
    .unwrap()
    .contention;
    assert_eq!(parsed.default_mode, ContentionMode::YieldFirst);
    assert_eq!(parsed.mode(ResourceKind::Cpu), ContentionMode::Performance);
    assert_eq!(parsed.mode(ResourceKind::Ram), ContentionMode::YieldFirst);
    assert_eq!(
        parsed
            .thresholds(ResourceKind::GpuMemory)
            .noise_tolerance_mib,
        512
    );
}

/// Every configuration key, found by driving the derived `Deserialize` impls one
/// field at a time: structs report their field lists, anything else is a leaf.
mod config_keys {
    use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, Visitor};
    use std::cell::RefCell;
    use std::collections::BTreeSet;
    use std::fmt;

    #[derive(Debug)]
    pub struct Stop;
    impl fmt::Display for Stop {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("stop")
        }
    }
    impl std::error::Error for Stop {}
    impl de::Error for Stop {
        fn custom<T: fmt::Display>(_: T) -> Self {
            Stop
        }
    }

    /// What one walk found at the end of its path.
    #[derive(Default)]
    pub struct Found {
        pub fields: Option<Vec<&'static str>>,
    }

    struct Walk<'a> {
        path: &'a [&'static str],
        depth: usize,
        found: &'a RefCell<Found>,
    }

    struct Fields<'a> {
        walk: Option<Walk<'a>>,
        key: Option<&'static str>,
    }

    impl<'de> MapAccess<'de> for Fields<'_> {
        type Error = Stop;
        fn next_key_seed<K: DeserializeSeed<'de>>(
            &mut self,
            seed: K,
        ) -> Result<Option<K::Value>, Stop> {
            match self.key.take() {
                Some(key) => seed
                    .deserialize(de::value::BorrowedStrDeserializer::<Stop>::new(key))
                    .map(Some),
                None => Ok(None),
            }
        }
        fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, Stop> {
            seed.deserialize(self.walk.take().ok_or(Stop)?)
        }
    }

    impl<'de> Deserializer<'de> for Walk<'_> {
        type Error = Stop;
        fn deserialize_any<V: Visitor<'de>>(self, _: V) -> Result<V::Value, Stop> {
            Err(Stop)
        }
        fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Stop> {
            visitor.visit_some(self)
        }
        fn deserialize_newtype_struct<V: Visitor<'de>>(
            self,
            _: &'static str,
            visitor: V,
        ) -> Result<V::Value, Stop> {
            visitor.visit_newtype_struct(self)
        }
        fn deserialize_struct<V: Visitor<'de>>(
            self,
            _: &'static str,
            fields: &'static [&'static str],
            visitor: V,
        ) -> Result<V::Value, Stop> {
            if self.depth == self.path.len() {
                self.found.borrow_mut().fields = Some(fields.to_vec());
                return Err(Stop);
            }
            let key = self.path[self.depth];
            visitor.visit_map(Fields {
                key: Some(key),
                walk: Some(Walk {
                    path: self.path,
                    depth: self.depth + 1,
                    found: self.found,
                }),
            })
        }
        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
            bytes byte_buf unit unit_struct seq tuple tuple_struct map enum identifier
            ignored_any
        }
    }

    /// Dotted paths of every leaf key of `T`.
    pub fn keys<T: for<'de> serde::Deserialize<'de>>() -> BTreeSet<String> {
        let mut leaves = BTreeSet::new();
        let mut queue: Vec<Vec<&'static str>> = vec![vec![]];
        while let Some(path) = queue.pop() {
            let found = RefCell::new(Found::default());
            let _ = T::deserialize(Walk {
                path: &path,
                depth: 0,
                found: &found,
            });
            match found.into_inner().fields {
                Some(fields) => {
                    for field in fields {
                        let mut child = path.clone();
                        child.push(field);
                        queue.push(child);
                    }
                }
                None => {
                    leaves.insert(path.join("."));
                }
            }
        }
        leaves
    }
}

/// A host for the floor and caution checks: RAM and GPUs by total memory.
fn host(total_ram_mib: u64, gpus: &[u64]) -> cedegrid::model::Snapshot {
    use cedegrid::model::*;
    Snapshot {
        schema_version: SCHEMA_VERSION,
        node_id: "local".into(),
        observed_at_unix_ms: 1,
        cpu_capacity_millicores: 8000,
        physical_cores: Some(4),
        cpu_busy_millicores: Some(0),
        cpu_busy_lower_millicores: None,
        managed_cpu_lower_millicores: None,
        total_ram_mib,
        available_ram_mib: Some(total_ram_mib),
        gpu_inventory: if gpus.is_empty() {
            CapabilityStatus::Unsupported
        } else {
            CapabilityStatus::Available
        },
        gpus: gpus
            .iter()
            .enumerate()
            .map(|(i, total)| GpuSnapshot {
                uuid: format!("GPU-{i}"),
                total_memory_mib: *total,
                used_memory_mib: Some(0),
                utilization_percent: None,
                external_process_ids: Some(vec![]),
                external_compute: ComputeActivity::Idle,
                compute_sample_id: None,
                observation: None,
                reserved_memory_mib: None,
                free_memory_mib: None,
                unattributed_memory_mib: None,
                external_compute_milli: None,
                own_compute_milli: None,
                compute_amount_unknown: None,
                own_only: false,
            })
            .collect(),
        capabilities: Default::default(),
        kernel: None,
        principals: None,
        external_processes: None,
    }
}

/// Every configuration key is classified as safe, bounded or caution
/// (constitution II); a key added without a class fails here.
#[test]
fn every_configuration_key_is_classified() {
    use config::KeyClass;
    let keys = config_keys::keys::<Config>();
    assert!(keys.len() > 100, "{keys:?}");
    let unclassified: Vec<_> = keys
        .iter()
        .filter(|key| config::key_class(key).is_none())
        .collect();
    assert!(
        unclassified.is_empty(),
        "unclassified keys: {unclassified:?}"
    );
    // No class names a key that does not exist, and every caution class names an
    // acknowledgement entry.
    for (pattern, class) in config::KEY_CLASSES {
        assert!(
            keys.iter().any(|key| config::key_pattern(key) == *pattern),
            "class for a missing key {pattern}"
        );
        if let KeyClass::Caution(entry) = class {
            assert!(
                config::ACKNOWLEDGEMENTS.contains(entry),
                "{pattern}: {entry}"
            );
        }
    }
    for (key, class) in [
        (
            "contention.cpu.neutral_margin_percent",
            KeyClass::Caution("reduced_margin"),
        ),
        (
            "contention.gpu_memory.noise_tolerance_mib",
            KeyClass::Caution("relaxed_detection"),
        ),
        ("contention.uid_min", KeyClass::Caution("relaxed_detection")),
        (
            "contention.gpu_compute.attribution_tolerance_percent",
            KeyClass::Caution("relaxed_detection"),
        ),
        ("contention.max_mode", KeyClass::Caution("performance_mode")),
        ("ram.reserve_mib", KeyClass::Caution("small_floor")),
        ("lifecycle.drain_timeout_ms", KeyClass::Bounded),
        ("contention.ram.hint_deadline_s", KeyClass::Bounded),
        ("cpu.nice", KeyClass::Safe),
    ] {
        assert_eq!(config::key_class(key), Some(class), "{key}");
    }
}

/// Each active caution option lists the keys and values it covers, for `doctor`
/// and setup.
#[test]
fn caution_details_list_the_keys_and_values_each_option_covers() {
    let config = parse_node(
        "[contention]\nacknowledge_caution = [\"performance_mode\", \"reduced_margin\", \"relaxed_detection\", \"small_floor\"]\nuid_min = 2000\n[contention.cpu]\nmode = \"performance\"\nneutral_margin_percent = 3\n[contention.gpu_memory]\nnoise_tolerance_mib = 512\n[contention.gpu_compute]\nattribution_tolerance_percent = 20\n[ram]\nreserve_mib = 600\nreserve_percent = 0",
    )
    .unwrap();
    let details = config.caution_details(&host(32_000, &[]));
    let settings = |name: &str| -> Vec<(String, String)> {
        details
            .iter()
            .find(|detail| detail.name == name)
            .unwrap_or_else(|| panic!("{name} missing: {details:?}"))
            .settings
            .iter()
            .map(|setting| (setting.key.clone(), setting.value.clone()))
            .collect()
    };
    let pairs = |list: &[(&str, &str)]| -> Vec<(String, String)> {
        list.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    };
    assert_eq!(
        settings("performance_mode"),
        pairs(&[("contention.cpu.mode", "performance")])
    );
    assert_eq!(
        settings("reduced_margin"),
        pairs(&[("contention.cpu.neutral_margin_percent", "3")])
    );
    assert_eq!(
        settings("relaxed_detection"),
        pairs(&[
            ("contention.uid_min", "2000"),
            ("contention.gpu_compute.attribution_tolerance_percent", "20"),
            ("contention.gpu_memory.noise_tolerance_mib", "512"),
        ])
    );
    assert_eq!(
        settings("small_floor"),
        pairs(&[("ram.reserve_mib", "600")])
    );
    // Without settings beyond their defaults, nothing is listed.
    assert!(
        parse_node("")
            .unwrap()
            .caution_details(&host(32_000, &[]))
            .is_empty()
    );
}

/// `noise_tolerance_mib` is at most 1024 MiB at load, and on the host at most
/// half of every GPU's effective floor; the agent refuses to start otherwise.
#[test]
fn noise_tolerance_is_bounded_at_load_and_against_each_gpu_floor() {
    let body = |noise: u64, floor: u64| {
        format!(
            "[contention]\nacknowledge_caution = [\"relaxed_detection\"]\n[contention.gpu_memory]\nnoise_tolerance_mib = {noise}\n[gpu]\nreserve_vram_mib = {floor}\nreserve_vram_percent = 0"
        )
    };
    assert!(parse_node(&body(1025, 4096)).is_err());
    assert!(parse_node(&body(1024, 4096)).is_ok());
    // A 1024 MiB floor allows at most 512 MiB.
    let tight = parse_node(&body(600, 1024)).unwrap();
    let error = format!(
        "{:#}",
        tight.validate_host(&host(32_000, &[8192])).unwrap_err()
    );
    assert!(error.contains("noise_tolerance_mib"), "{error}");
    assert!(tight.validate_host(&host(32_000, &[])).is_ok());
    let roomy = parse_node(&body(600, 2048)).unwrap();
    assert!(roomy.validate_host(&host(32_000, &[8192, 24_576])).is_ok());
    // The default tolerance fits every default floor.
    assert!(
        parse_node("")
            .unwrap()
            .validate_host(&host(32_000, &[8192, 24_576, 81_920]))
            .is_ok()
    );
}

/// GPU memory admission waits for its own cooldown: a set
/// `contention.gpu_memory.scale_up_cooldown_ms` wins, else a set legacy
/// `gpu.scale_up_cooldown_ms`, else the mode's scale-up interval (FR-034).
#[test]
fn gpu_memory_cooldown_precedence() {
    use cedegrid::contention::ContentionMode::*;
    let defaults = parse_node("").unwrap();
    assert_eq!(defaults.gpu_memory_cooldown_ms(Neutral), 10_000);
    assert_eq!(defaults.gpu_memory_cooldown_ms(YieldFirst), 30_000);
    let legacy = parse_node("[gpu]\nscale_up_cooldown_ms = 45000").unwrap();
    assert_eq!(legacy.gpu_memory_cooldown_ms(Neutral), 45_000);
    let both = parse_node(
        "[gpu]\nscale_up_cooldown_ms = 45000\n[contention.gpu_memory]\nscale_up_cooldown_ms = 2000",
    )
    .unwrap();
    assert_eq!(both.gpu_memory_cooldown_ms(Neutral), 2000);
    assert!(parse_node("[contention.gpu_compute]\nscale_up_cooldown_ms = 2000").is_err());
}

/// The GPU compute attribution tolerance is bounded at 1-30 points and applies to
/// GPU compute only (FR-021).
#[test]
fn the_gpu_compute_attribution_tolerance_is_bounded() {
    let body = |section: &str, value: u32| {
        format!("[contention.{section}]\nattribution_tolerance_percent = {value}")
    };
    assert!(parse_node(&body("gpu_compute", 0)).is_err());
    assert!(parse_node(&body("gpu_compute", 31)).is_err());
    assert!(parse_node(&body("cpu", 15)).is_err());
    // Above the default it trusts less consistent samples: relaxed detection.
    let config = parse_node(&format!(
        "[contention]\nacknowledge_caution = [\"relaxed_detection\"]\n{}",
        body("gpu_compute", 30)
    ))
    .unwrap();
    assert_eq!(
        config
            .contention
            .thresholds(cedegrid::contention::ResourceKind::GpuCompute)
            .attribution_tolerance_percent,
        30
    );
    assert_eq!(
        parse_node("")
            .unwrap()
            .contention
            .thresholds(cedegrid::contention::ResourceKind::GpuCompute)
            .attribution_tolerance_percent,
        15
    );
}

/// Settings that lengthen reclaim or let stale telemetry pass are bounded.
#[test]
fn settings_that_lengthen_reclaim_are_bounded() {
    for bad in [
        "[monitor]\ninterval_ms = 10001",
        "[lifecycle]\ndrain_timeout_ms = 60001",
        "[lifecycle]\nterm_grace_ms = 60001",
        "[kernel]\nmonitor_interval_ms = 10001",
        "[kernel]\nfreshness_intervals = 21",
        "[gpu]\nprocess_sample_max_age_ms = 60001",
    ] {
        assert!(parse_node(bad).is_err(), "accepted: {bad}");
    }
    for good in [
        "[monitor]\ninterval_ms = 10000",
        "[lifecycle]\ndrain_timeout_ms = 60000\nterm_grace_ms = 60000",
        "[kernel]\nmonitor_interval_ms = 10000\nfreshness_intervals = 20",
        "[gpu]\nprocess_sample_max_age_ms = 60000",
    ] {
        assert!(parse_node(good).is_ok(), "rejected: {good}");
    }
}

/// Settings that let own work hold more capacity than their defaults are caution
/// options (constitution II): a margin below its default, or a saturation,
/// contention-floor or noise threshold above it.
#[test]
fn margins_and_detection_thresholds_beyond_their_defaults_need_acknowledgement() {
    for (setting, option) in [
        (
            "[contention.cpu]\nneutral_margin_percent = 3",
            "reduced_margin",
        ),
        (
            "[contention.cpu]\nperformance_margin_percent = 1",
            "reduced_margin",
        ),
        (
            "[contention.gpu_compute]\nyield_first_margin_percent = 12",
            "reduced_margin",
        ),
        (
            "[contention.ram]\nyield_first_margin_percent = 15",
            "reduced_margin",
        ),
        (
            "[contention.cpu]\nsaturation_percent = 99",
            "relaxed_detection",
        ),
        (
            "[contention.cpu]\nprincipal_min_percent = 2",
            "relaxed_detection",
        ),
        (
            "[contention.gpu_memory]\nnoise_tolerance_mib = 512",
            "relaxed_detection",
        ),
        ("[contention]\nuid_min = 2000", "relaxed_detection"),
        (
            "[contention.ram]\nmemory_peak_window_s = 100",
            "relaxed_detection",
        ),
        (
            "[contention.gpu_compute]\nquiet_window_intervals = 10",
            "relaxed_detection",
        ),
        (
            "[contention.gpu_compute]\nattribution_tolerance_percent = 20",
            "relaxed_detection",
        ),
        (
            "[contention.cpu]\npsi_some_percent = 30",
            "relaxed_detection",
        ),
    ] {
        let error = format!("{:#}", parse_node(setting).unwrap_err());
        assert!(
            error.contains("ERR_CEDEGRID_CAUTION_UNACKNOWLEDGED") && error.contains(option),
            "{setting}: {error}"
        );
        let acknowledged = format!("[contention]\nacknowledge_caution = [\"{option}\"]\n");
        let config = parse_node(&match setting.strip_prefix("[contention]\n") {
            Some(rest) => format!("{acknowledged}{rest}"),
            None => format!("{acknowledged}{setting}"),
        })
        .unwrap();
        assert!(
            config.caution_options().iter().any(|o| o.name == option),
            "{setting}"
        );
    }
    // Settings that leave more to external demand need nothing.
    for setting in [
        "[contention.cpu]\nperformance_margin_percent = 4",
        "[contention.cpu]\nyield_first_margin_percent = 40",
        "[contention.cpu]\nsaturation_percent = 90",
        "[contention.gpu_memory]\nnoise_tolerance_mib = 128",
        "[contention]\nuid_min = 500",
        "[contention.ram]\nmemory_peak_window_s = 600",
        "[contention.gpu_compute]\nquiet_window_intervals = 40",
        "[contention.gpu_compute]\nattribution_tolerance_percent = 10",
        "[contention.cpu]\npsi_some_percent = 5",
    ] {
        assert!(
            parse_node(setting).unwrap().caution_options().is_empty(),
            "{setting}"
        );
    }
}

#[test]
fn contention_validation_rejects_out_of_range_and_unknown_settings() {
    for bad in [
        "[contention]\ndefault_mode = \"performance\"\nmax_mode = \"neutral\"",
        "[contention]\nmax_mode = \"neutral\"\n[contention.cpu]\nmode = \"performance\"",
        "[contention.cpu]\nyield_first_margin_percent = 0",
        "[contention.cpu]\nyield_first_margin_percent = 91",
        "[contention.cpu]\nyield_first_margin_percent = 10\nperformance_margin_percent = 10",
        "[contention.ram]\nneutral_tolerance_points = 0",
        "[contention.ram]\nneutral_tolerance_points = 51",
        "[contention.cpu]\nhysteresis_percent = 0",
        "[contention.cpu]\nhysteresis_percent = 100",
        "[contention.gpu_compute]\nquiet_window_intervals = 0",
        "[contention.gpu_compute]\nquiet_window_intervals = 2",
        "[contention]\nbogus = 1",
        "[contention.cpu]\nbogus = 1",
        "[contention.cpu]\nnoise_tolerance_mib = 5",
        "[contention]\nacknowledge_caution = [\"something_else\"]",
        "[contention]\nacknowledge_caution = [\"floor_policy_c\"]",
    ] {
        assert!(parse_node(bad).is_err(), "accepted: {bad}");
    }
}

/// Constitution 2.0.0: options that put other workloads at risk no longer exist.
#[test]
fn removed_floor_and_memory_policies_fail_naming_their_replacement() {
    for (body, replacement) in [
        (
            "[contention]\nfloor_policy = \"b\"",
            "contention.guaranteed_hint_deadline_s",
        ),
        (
            "[contention]\nfloor_policy = \"c\"\nacknowledge_caution = [\"floor_policy_c\"]",
            "contention.ram.guaranteed_breach_window_s",
        ),
        (
            "[contention]\nneutral_memory_policy = \"b\"",
            "ram.reserve_mib",
        ),
        (
            "[contention]\nneutral_memory_policy = \"b\"",
            "gpu.reserve_vram_percent",
        ),
    ] {
        let error = format!("{:#}", parse_node(body).unwrap_err());
        assert!(
            error.contains("was removed") && error.contains(replacement),
            "{body}: {error}"
        );
    }
    parse_node("[contention]\nfloor_policy = \"a\"\nneutral_memory_policy = \"a\"").unwrap();
}

#[test]
fn performance_needs_its_acknowledgement() {
    for (body, acknowledged) in [
        (
            "[contention]\ndefault_mode = \"performance\"",
            "[contention]\ndefault_mode = \"performance\"\nacknowledge_caution = [\"performance_mode\"]",
        ),
        (
            "[contention.gpu_memory]\nmode = \"performance\"",
            "[contention]\nacknowledge_caution = [\"performance_mode\"]\n[contention.gpu_memory]\nmode = \"performance\"",
        ),
    ] {
        let error = format!("{:#}", parse_node(body).unwrap_err());
        assert!(
            error.contains("ERR_CEDEGRID_CAUTION_UNACKNOWLEDGED")
                && error.contains("performance_mode"),
            "{body}: {error}"
        );
        parse_node(acknowledged).unwrap();
    }
}

#[test]
fn floors_are_raised_to_their_minimum_and_small_floors_warn() {
    let (_root, location) = node_location();
    let load = |body: &str| {
        Config::parse_reporting(&format!("config_version = 1\n{body}\n"), &location).unwrap()
    };
    // Below the minimum (512 MiB of RAM, 512 MiB per GPU), a floor is raised to it;
    // near the minimum and unconfirmed, to twice the minimum, so a caution option is
    // never active unconfirmed.
    let small_floors = "[ram]\nreserve_mib = 100\nreserve_percent = 0\n[gpu]\nreserve_vram_mib = 100\nreserve_vram_percent = 0";
    let raised = load(small_floors);
    assert_eq!(raised.config.ram_floor_mib(32_000), 1024);
    assert_eq!(raised.config.vram_floor_mib(16_000), 1024);
    let warned = |loaded: &cedegrid::config::LoadedConfig, words: &[&str]| {
        loaded
            .warnings
            .iter()
            .any(|w| words.iter().all(|word| w.contains(word)))
    };
    assert!(warned(&raised, &["ram.reserve_mib", "512"]));
    assert!(warned(&raised, &["gpu.reserve_vram_mib", "512"]));
    assert!(warned(
        &raised,
        &["acknowledge_caution = [\"small_floor\"]"]
    ));
    assert!(raised.config.caution_options().is_empty());
    assert!(raised.config.ram_floor(32_000).raised);
    let acknowledged = load(&format!(
        "{small_floors}\n[contention]\nacknowledge_caution = [\"small_floor\"]"
    ));
    assert_eq!(acknowledged.config.ram_floor_mib(32_000), 512);
    assert_eq!(acknowledged.config.vram_floor_mib(16_000), 512);
    assert!(!warned(
        &acknowledged,
        &["acknowledge_caution = [\"small_floor\"]"]
    ));
    assert!(
        acknowledged
            .config
            .caution_options()
            .iter()
            .any(|option| option.name == "small_floor")
    );
    // A floor near the minimum still loads, so 0.2 files keep loading.
    let small = load("[ram]\nreserve_mib = 800\nreserve_percent = 0");
    assert_eq!(small.config.ram_floor_mib(32_000), 1024);
    // A percentage floor can exceed the minimum on a large host.
    let percent = load("[ram]\nreserve_mib = 0\nreserve_percent = 5");
    assert_eq!(percent.config.ram_floor_mib(32_000), 1600);
    // The default GPU floor is 5% of the device, at least twice the minimum, so a
    // default is never a near-minimum caution floor; 3 GiB applies only when set.
    for (total, floor) in [(8192, 1024), (16_000, 1024), (24_576, 1229), (81_920, 4096)] {
        assert_eq!(Config::default().vram_floor_mib(total), floor, "{total}");
        assert!(!Config::default().vram_floor(total).raised, "{total}");
    }
    let fixed = load("[gpu]\nreserve_vram_mib = 3072");
    assert_eq!(fixed.config.vram_floor_mib(24_576), 3072);
    assert_eq!(Config::default().ram_floor_mib(64_000), 8192);
}

/// A floor sized by a percentage is judged on the host: near its minimum it is a
/// caution option when confirmed and raised when not (FR-023, FR-043).
#[test]
fn a_percentage_floor_near_its_minimum_is_judged_on_the_host() {
    use cedegrid::model::*;
    let (_root, location) = node_location();
    let load = |body: &str| {
        Config::parse_reporting(&format!("config_version = 1\n{body}\n"), &location)
            .unwrap()
            .config
    };
    let snapshot = |total_ram_mib| Snapshot {
        schema_version: SCHEMA_VERSION,
        node_id: "local".into(),
        observed_at_unix_ms: 1,
        cpu_capacity_millicores: 8000,
        physical_cores: Some(4),
        cpu_busy_millicores: Some(0),
        cpu_busy_lower_millicores: None,
        managed_cpu_lower_millicores: None,
        total_ram_mib,
        available_ram_mib: Some(total_ram_mib),
        gpu_inventory: CapabilityStatus::Unsupported,
        gpus: vec![],
        capabilities: Default::default(),
        kernel: None,
        principals: None,
        external_processes: None,
    };
    // 2% of 32000 MiB is 640 MiB: near the minimum.
    let body = "[ram]\nreserve_mib = 0\nreserve_percent = 2";
    let unconfirmed = load(body);
    let floors = unconfirmed.floors(&snapshot(32_000));
    assert!(floors.ram.near_minimum && floors.ram.raised);
    assert_eq!(floors.ram.effective_mib, 1024);
    assert!(unconfirmed.caution_options_on(&snapshot(32_000)).is_empty());
    let confirmed = load(&format!(
        "{body}\n[contention]\nacknowledge_caution = [\"small_floor\"]"
    ));
    assert_eq!(confirmed.floors(&snapshot(32_000)).ram.effective_mib, 640);
    assert!(
        confirmed
            .caution_options_on(&snapshot(32_000))
            .iter()
            .any(|option| option.name == "small_floor")
    );
    // On a larger host the same setting is not near the minimum.
    assert!(!confirmed.floors(&snapshot(128_000)).ram.near_minimum);
    // The load cannot judge a percentage floor; the agent warns once it can.
    let warnings = unconfirmed.host_floor_warnings(&snapshot(32_000));
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("1024 MiB"), "{warnings:?}");
    assert!(
        unconfirmed
            .host_floor_warnings(&snapshot(128_000))
            .is_empty()
    );
    assert!(confirmed.host_floor_warnings(&snapshot(32_000)).is_empty());
    // A floor the settings alone decide is warned about at load instead.
    let fixed = load("[ram]\nreserve_mib = 600\nreserve_percent = 0");
    assert!(fixed.floors(&snapshot(32_000)).ram.raised);
    assert!(fixed.host_floor_warnings(&snapshot(32_000)).is_empty());
}

#[test]
fn guaranteed_memory_knobs_are_validated() {
    for bad in [
        "[contention]\nguaranteed_hint_deadline_s = 0",
        "[contention]\nguaranteed_hint_deadline_s = 31",
        "[contention.ram]\nguaranteed_breach_window_s = 61",
        "[contention.gpu_memory]\nguaranteed_breach_window_s = 5",
        "[contention]\ncritical_floor_percent = 5",
        "[contention]\ncritical_floor_percent = 95",
        "[contention]\ncritical_hint_deadline_s = 0",
        "[contention]\ncritical_hint_deadline_s = 6",
        "[contention]\nguaranteed_hint_deadline_s = 2\ncritical_hint_deadline_s = 3",
        "[contention.ram]\nhint_deadline_s = 31",
        "[contention.cpu]\nunknown_window_intervals = 21",
        "[contention.cpu]\nsaturation_percent = 49",
        "[contention.ram]\nsaturation_percent = 90",
        "[contention.cpu]\nneutral_margin_percent = 26",
        "[contention.cpu]\nneutral_margin_percent = 1",
        "[contention.ram]\nneutral_margin_percent = 20",
        "[contention.cpu]\nmin_step_dwell_intervals = 11",
        "[contention.cpu]\nyield_first_response_intervals = 11",
        "[contention.cpu]\nreclaim_s = 61",
        "[contention.cpu]\npause_bound_s = 61",
        "[contention.ram]\nreclaim_s = 5",
        "[contention.cpu]\nadmission_usage_window_s = 0",
        "[contention.cpu]\nadmission_usage_window_s = 3601",
        "[contention.ram]\nadmission_usage_window_s = 60",
        "[contention.cpu]\npsi_some_percent = 0",
        "[contention.cpu]\npsi_some_percent = 101",
        "[contention.gpu_compute]\npsi_some_percent = 10",
        "[contention.ram]\npsi_some_percent = 10",
    ] {
        assert!(parse_node(bad).is_err(), "accepted: {bad}");
    }
    for good in [
        "[contention]\nguaranteed_hint_deadline_s = 30",
        "[contention.ram]\nguaranteed_breach_window_s = 0",
        "[contention.ram]\nguaranteed_breach_window_s = 60",
    ] {
        parse_node(good).unwrap();
    }
}

#[test]
fn negative_cpu_nice_warns_and_is_treated_as_zero() {
    let (_root, location) = node_location();
    let loaded =
        Config::parse_reporting("config_version = 1\n[cpu]\nnice = -5\n", &location).unwrap();
    assert_eq!(loaded.config.cpu.nice, 0);
    assert!(loaded.warnings.iter().any(|w| w.contains("cpu.nice")));
    let loaded = Config::parse_reporting(
        "config_version = 1\n[contention]\ndefault_mode = \"performance\"\nacknowledge_caution = [\"performance_mode\"]\n",
        &location,
    )
    .unwrap();
    assert!(
        loaded
            .warnings
            .iter()
            .any(|w| w.contains("performance_mode") && w.contains("caution"))
    );
}

/// Legacy `gpu.execution_mode` values are interpreted with a warning naming the
/// replacement, for GPU resources without an explicit mode (FR-031).
#[test]
fn legacy_gpu_execution_modes_are_interpreted_with_a_warning() {
    use cedegrid::contention::{ContentionMode, ResourceKind};
    let (_root, location) = node_location();
    let load = |body: &str| {
        Config::parse_reporting(&format!("config_version = 1\n{body}\n"), &location).unwrap()
    };
    let legacy = |loaded: &cedegrid::config::LoadedConfig| {
        loaded
            .warnings
            .iter()
            .filter(|w| w.contains("gpu."))
            .cloned()
            .collect::<Vec<_>>()
    };
    for body in ["", "[gpu]\nexecution_mode = \"auto\""] {
        let auto = load(body);
        assert!(legacy(&auto).is_empty(), "{body}");
        assert!(auto.config.legacy_gpu().warnings.is_empty());
    }
    let aware = load(
        "[contention]\ndefault_mode = \"yield_first\"\n[gpu]\nexecution_mode = \"contention_aware\"",
    );
    assert!(
        legacy(&aware)
            .iter()
            .any(|w| w.contains("contention_aware") && w.contains("contention.gpu_compute.mode")),
        "{:?}",
        aware.warnings
    );
    let mode = |loaded: &cedegrid::config::LoadedConfig, kind| loaded.config.contention.mode(kind);
    assert_eq!(
        mode(&aware, ResourceKind::GpuCompute),
        ContentionMode::Neutral
    );
    assert_eq!(
        mode(&aware, ResourceKind::GpuMemory),
        ContentionMode::Neutral
    );
    assert_eq!(mode(&aware, ResourceKind::Cpu), ContentionMode::YieldFirst);
    // An explicit mode wins over the legacy value.
    let explicit = load(
        "[contention.gpu_compute]\nmode = \"yield_first\"\n[gpu]\nexecution_mode = \"contention_aware\"",
    );
    assert_eq!(
        mode(&explicit, ResourceKind::GpuCompute),
        ContentionMode::YieldFirst
    );
    assert_eq!(
        mode(&explicit, ResourceKind::GpuMemory),
        ContentionMode::Neutral
    );
    let conservative = load("[gpu]\nexecution_mode = \"conservative_non_sharing\"");
    assert!(conservative.config.contention.never_share_occupied_gpus);
    assert!(
        legacy(&conservative)
            .iter()
            .any(|w| w.contains("contention.never_share_occupied_gpus"))
    );
    let best_effort = load(
        "[gpu]\nexecution_mode = \"best_effort_occupied\"\n[[gpu.best_effort_external_processes.GPU-x]]\npid = 4242\nboot_id = \"boot\"\nstart_ticks = 1\nuid = 1000",
    );
    assert_eq!(
        mode(&best_effort, ResourceKind::GpuMemory),
        ContentionMode::Neutral
    );
    assert!(
        legacy(&best_effort)
            .iter()
            .any(|w| w.contains("best_effort_occupied") && w.contains("no longer used"))
    );
    assert!(!best_effort.config.legacy_gpu().interpretation.is_empty());
}

/// The supervisor writes the target files the SDKs read: a fresh channel with an
/// allowance writes exactly TF-01, and every valid case round-trips through the
/// writer's type (contracts/resource-target-file.md, FR-071).
#[test]
fn the_supervisor_writes_the_target_files_the_sdks_read() {
    use cedegrid::actuation::{ResourceTarget, TARGET_FILE, TargetChannel};
    let cases = contract()["target_file_cases"].as_array().unwrap().clone();
    let case = |id: &str| cases.iter().find(|c| c["id"] == id).unwrap().clone();
    let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let mut channel = TargetChannel::new(5000);
    let tf01 = case("TF-01");
    assert!(
        channel.set_allowance(serde_json::from_value(tf01["file"]["allowance"].clone()).unwrap())
    );
    channel.publish_allowance(&[dir.path().to_owned()]).unwrap();
    let written: Value =
        serde_json::from_slice(&fs::read(dir.path().join(TARGET_FILE)).unwrap()).unwrap();
    assert_eq!(written, tf01["file"]);
    for id in ["TF-01", "TF-02", "TF-03"] {
        let file = case(id)["file"].clone();
        let target: ResourceTarget = serde_json::from_value(file.clone()).unwrap();
        assert_eq!(serde_json::to_value(&target).unwrap(), file, "{id}");
    }
}

/// The shipped node example and the 0.2 node example load unchanged, and without
/// mode settings every resource runs Neutral (FR-031).
#[test]
fn the_shipped_and_0_2_node_configs_load_unchanged_in_neutral() {
    use cedegrid::contention::{ContentionMode, ResourceKind};
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for path in [
        root.join("examples/node.toml"),
        root.join("tests/fixtures/0.2/node.toml"),
    ] {
        let config = Config::load(&path).unwrap_or_else(|e| panic!("{}: {e:#}", path.display()));
        for kind in ResourceKind::ALL {
            assert_eq!(
                config.contention.mode(kind),
                ContentionMode::Neutral,
                "{}",
                path.display()
            );
        }
        assert_eq!(config.contention.max_mode(), ContentionMode::Neutral);
    }
}

/// Thresholds are only built from defaults and configuration, never
/// deserialized, so CPU and GPU compute are always compute and RAM and GPU memory
/// never are, whatever a file sets, for the shipped, 0.2 and fully set configs.
/// Performance's per-interval compute scale-up depends on it (FR-034).
#[test]
fn thresholds_mark_compute_by_resource_kind_on_every_path() {
    use cedegrid::contention::{ContentionMode, ResourceKind, ResourceThresholds};
    let compute = |kind: ResourceKind| matches!(kind, ResourceKind::Cpu | ResourceKind::GpuCompute);
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut configs = vec![
        Config::default(),
        Config::load(&root.join("examples/node.toml")).unwrap(),
        Config::load(&root.join("tests/fixtures/0.2/node.toml")).unwrap(),
    ];
    let mut body = String::from(
        "[contention]\ndefault_mode = \"performance\"\nacknowledge_caution = [\"performance_mode\"]\n",
    );
    for section in ["cpu", "ram", "gpu_compute", "gpu_memory"] {
        body.push_str(&format!(
            "[contention.{section}]\nscale_up_step_interval_s = 5\nperformance_ramp_s = 20\n"
        ));
    }
    configs.push(parse_node(&body).unwrap());
    for config in &configs {
        for kind in ResourceKind::ALL {
            let thresholds = config.contention.thresholds(kind);
            assert_eq!(thresholds.compute, compute(kind), "{kind:?}");
            assert_eq!(ResourceThresholds::defaults(kind).compute, compute(kind));
        }
    }
    // With no override, Performance scales compute up every interval and memory every 2 s.
    let interval = Config::default().monitor.interval_ms;
    for kind in ResourceKind::ALL {
        let expected = if compute(kind) { interval } else { 2000 };
        assert_eq!(
            configs[0]
                .contention
                .thresholds(kind)
                .scale_up_interval_ms(ContentionMode::Performance, interval),
            expected,
            "{kind:?}"
        );
    }
    // No file can set it.
    for section in ["cpu", "ram", "gpu_compute", "gpu_memory"] {
        assert!(parse_node(&format!("[contention.{section}]\ncompute = false")).is_err());
    }
}

/// A negotiated heartbeat with per-resource admission, held-back amounts,
/// allocation readiness and the worker limit parses and round-trips; the 0.2
/// report fixture still parses (FR-032).
#[test]
fn negotiated_report_fields_round_trip_beside_the_0_2_report() {
    use cedegrid::protocol::Request;
    for case in contract()["negotiated_report_cases"].as_array().unwrap() {
        let request: Request = serde_json::from_value(case["request"].clone())
            .unwrap_or_else(|e| panic!("{}: {e}", case["id"]));
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            case["request"],
            "{}",
            case["id"]
        );
    }
    let old: Request = serde_json::from_str(include_str!("fixtures/0.2/node_report.json")).unwrap();
    assert!(matches!(old, Request::Heartbeat { .. }));
}

/// Per-job modes parse and validate alike in Rust and both SDKs (FR-030).
#[test]
fn job_contention_cases_parse_and_validate_alike() {
    use cedegrid::execution_model::LaunchRequest;
    let reply: Value =
        serde_json::from_str(include_str!("fixtures/0.2/heartbeat_reply.json")).unwrap();
    let base = reply["assignments"][0]["request"].clone();
    for case in contract()["job_contention_cases"].as_array().unwrap() {
        let mut task = base.clone();
        task["contention"] = json!({"modes": case["modes"]});
        let accepted = serde_json::from_value::<LaunchRequest>(task.clone())
            .map_err(anyhow::Error::from)
            .and_then(|request| {
                cedegrid::protocol::validate_launch_request(&request)?;
                Ok(request)
            });
        assert_eq!(
            accepted.is_ok(),
            case["accepted"].as_bool().unwrap(),
            "{}: {accepted:?}",
            case["id"]
        );
        if let Ok(request) = accepted {
            assert_eq!(
                serde_json::to_value(&request).unwrap()["contention"],
                task["contention"]
            );
        }
    }
}

/// No configuration can name Aggressive: the mode is not implemented, and its
/// unprotected form may never come from configuration (constitution II, 4.0.0).
/// The job field is pinned by the contract's JC-05 in Rust and both SDKs.
#[test]
fn no_configuration_can_name_aggressive() {
    for body in [
        "[contention]\ndefault_mode = \"aggressive\"",
        "[contention]\nmax_mode = \"aggressive\"",
        "[contention.cpu]\nmode = \"aggressive\"",
        "[contention.gpu_compute]\nmode = \"aggressive\"",
    ] {
        assert!(parse_node(body).is_err(), "{body}");
    }
}

/// Memory keeps only the floor under Neutral and Performance, so those margins do
/// not exist for RAM and GPU memory.
#[test]
fn memory_has_no_neutral_or_performance_margin() {
    for resource in ["ram", "gpu_memory"] {
        for key in ["neutral_margin_percent", "performance_margin_percent"] {
            let error = format!(
                "{:#}",
                parse_node(&format!("[contention.{resource}]\n{key} = 5")).unwrap_err()
            );
            assert!(
                error.contains("keeps only the floor under Neutral and Performance"),
                "{resource}.{key}: {error}"
            );
        }
    }
    // Yield First's memory margin is not ordered against margins that do not apply.
    let config = parse_node(
        "[contention]\nacknowledge_caution = [\"reduced_margin\"]\n[contention.ram]\nyield_first_margin_percent = 3",
    )
    .unwrap();
    assert_eq!(
        config
            .contention
            .thresholds(cedegrid::contention::ResourceKind::Ram)
            .yield_first_margin_percent,
        3
    );
}
