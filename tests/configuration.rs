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
    assert_eq!(executed, 11);
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
