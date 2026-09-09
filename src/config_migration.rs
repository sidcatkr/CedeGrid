//! Explicit legacy-config conversion. Runtime loaders never fall back to YAML/JSON.
use crate::config::{
    CONFIG_ERROR, CONFIG_VERSION, RuntimeConfigKind, normalize_effective, parse_runtime,
    serialize_runtime,
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
#[cfg(not(windows))]
use std::fs::File;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum LegacyClientSemantics {
    Rust,
    Python,
}

#[derive(Debug, Clone)]
pub struct MigrationOptions {
    pub kind: RuntimeConfigKind,
    pub input: PathBuf,
    pub output: PathBuf,
    pub legacy_client_semantics: Option<LegacyClientSemantics>,
    pub legacy_cwd: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
pub struct MigrationReport {
    pub config_version: u32,
    pub kind: RuntimeConfigKind,
    pub input: PathBuf,
    pub output: PathBuf,
    pub input_sha256: String,
    pub output_sha256: String,
    pub effective_settings_preserved: bool,
    pub atomic_no_clobber: bool,
    pub output_directory_synchronized: bool,
}

/// The output exists after this error. Retain/query this original path and hash;
/// rerunning conversion must not replace it or claim that no publication occurred.
#[derive(Debug)]
pub struct MigrationPublicationUncertain {
    pub output: PathBuf,
    pub output_sha256: String,
    pub reason: String,
}

impl std::fmt::Display for MigrationPublicationUncertain {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "ERR_CEDEGRID_PUBLICATION_UNCERTAIN: config output {} was published with SHA256 {}; retain it and verify directory durability: {}",
            self.output.display(),
            self.output_sha256,
            self.reason
        )
    }
}
impl std::error::Error for MigrationPublicationUncertain {}

pub fn migrate(options: &MigrationOptions) -> Result<MigrationReport> {
    let prepared =
        prepare(options).map_err(|error| anyhow::anyhow!("{CONFIG_ERROR}: {error:#}"))?;
    publish(&prepared.output, prepared.text.as_bytes(), sync_directory)?;
    Ok(MigrationReport {
        config_version: CONFIG_VERSION,
        kind: options.kind,
        input: prepared.input,
        output: prepared.output,
        input_sha256: prepared.input_sha256,
        output_sha256: hex::encode(Sha256::digest(prepared.text.as_bytes())),
        effective_settings_preserved: true,
        atomic_no_clobber: true,
        output_directory_synchronized: true,
    })
}

struct Prepared {
    input: PathBuf,
    output: PathBuf,
    input_sha256: String,
    text: String,
}

fn prepare(options: &MigrationOptions) -> Result<Prepared> {
    match (
        options.kind,
        options.legacy_client_semantics,
        &options.legacy_cwd,
    ) {
        (RuntimeConfigKind::Client, Some(LegacyClientSemantics::Python), Some(cwd)) => {
            ensure!(
                cwd.is_absolute(),
                "Python legacy semantics requires an absolute --legacy-cwd"
            );
        }
        (RuntimeConfigKind::Client, Some(LegacyClientSemantics::Rust), None) => {}
        (RuntimeConfigKind::Client, _, _) => anyhow::bail!(
            "client migration requires --legacy-client-semantics rust|python; Python requires an absolute --legacy-cwd and Rust accepts no legacy CWD"
        ),
        (_, None, None) => {}
        _ => anyhow::bail!("legacy client options apply only to client migration"),
    }
    let input = options
        .input
        .canonicalize()
        .context("cannot resolve legacy input")?;
    ensure!(input.is_file(), "legacy input must be a regular file");
    let bytes = fs::read(&input).context("cannot read legacy input")?;
    let text = std::str::from_utf8(&bytes).context("legacy input is not UTF-8")?;
    // serde_yaml's mapping visitor rejects duplicate keys, also for JSON syntax.
    let legacy: serde_yaml::Value =
        serde_yaml::from_str(text).context("invalid legacy YAML/JSON")?;
    let mut legacy = serde_json::to_value(legacy)
        .context("legacy settings are not representable JSON values")?;
    ensure!(legacy.is_object(), "legacy configuration must be an object");
    ensure!(
        legacy.get("config_version").is_none(),
        "input already contains a runtime config_version"
    );
    let input_base = input.parent().context("legacy config has no parent")?;

    let legacy_base = if options.kind == RuntimeConfigKind::Client {
        match options.legacy_client_semantics.expect("checked above") {
            LegacyClientSemantics::Rust => {
                let settings: crate::protocol::ClientConfig = serde_json::from_value(legacy)
                    .context("invalid legacy Rust client settings")?;
                legacy = serde_json::to_value(settings)?;
                let object = legacy.as_object_mut().unwrap();
                // The audited Rust transport used a ten-second request timeout
                // and no transfer pacer. Do not silently adopt the new defaults.
                object.insert("timeout_seconds".into(), Value::from(10.0));
                object.insert("max_transfer_bytes_per_second".into(), Value::from(0));
                input_base
            }
            LegacyClientSemantics::Python => options.legacy_cwd.as_deref().unwrap(),
        }
    } else {
        input_base
    };
    if options.kind == RuntimeConfigKind::Node {
        let object = legacy.as_object_mut().unwrap();
        let schema = object.get("schema_version").map(|version| version.as_u64());
        ensure!(
            matches!(schema, None | Some(Some(1 | 2))),
            "unsupported legacy node schema_version"
        );
        object.entry("schema_version").or_insert(Value::from(2));
        object
            .entry("state_dir")
            .or_insert(Value::from(".resource-manager-state"));
    }
    let effective = normalize_effective(legacy, options.kind, legacy_base)?;

    let absolute_output = std::path::absolute(&options.output)?;
    let parent = absolute_output
        .parent()
        .context("output needs a parent directory")?
        .canonicalize()
        .context("output parent must already exist")?;
    ensure!(parent.is_dir(), "output parent must be a directory");
    let output = parent.join(
        absolute_output
            .file_name()
            .context("output needs a file name")?,
    );
    match fs::symlink_metadata(&output) {
        Ok(_) => anyhow::bail!("output already exists; conversion never overwrites"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let converted = serialize_runtime(&effective, options.kind)?;
    let roundtrip: Value = parse_runtime(&converted, &output, options.kind)?;
    ensure!(
        effective == roundtrip,
        "converted configuration changed normalized effective settings"
    );
    Ok(Prepared {
        input,
        output,
        input_sha256: hex::encode(Sha256::digest(&bytes)),
        text: converted,
    })
}

fn publish(
    output: &Path,
    bytes: &[u8],
    sync_parent: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Result<()> {
    let parent = output.parent().context("output has no parent")?;
    let temporary = parent.join(format!(".cedegrid-config-{}.tmp", uuid::Uuid::new_v4()));
    let before_publication = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        }
        let mut file = options
            .open(&temporary)
            .context("create private config temporary file")?;
        file.write_all(bytes).context("write converted config")?;
        file.sync_all()
            .context("sync converted config before publication")?;
        drop(file);
        // Same-directory hard-link creation is atomic and fails if *any* target
        // exists, including a symlink. Never use an overwrite-capable rename.
        fs::hard_link(&temporary, output).context("atomic no-clobber config publication")?;
        Ok(())
    })();
    if let Err(error) = before_publication {
        let _ = fs::remove_file(&temporary);
        return Err(anyhow::anyhow!("{CONFIG_ERROR}: {error:#}"));
    }
    let cleanup = fs::remove_file(&temporary);
    let synchronization = sync_parent(parent);
    if let Err(error) = synchronization {
        return Err(MigrationPublicationUncertain {
            output: output.to_path_buf(),
            output_sha256: hex::encode(Sha256::digest(bytes)),
            reason: error.to_string(),
        }
        .into());
    }
    // Publication is durable even if removal of our secondary temporary name
    // failed. Report its preserved path rather than claim a clean conversion.
    if let Err(error) = cleanup {
        return Err(MigrationPublicationUncertain {
            output: output.to_path_buf(),
            output_sha256: hex::encode(Sha256::digest(bytes)),
            reason: format!("temporary name {} remains: {error}", temporary.display()),
        }
        .into());
    }
    Ok(())
}

fn sync_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Request a directory handle. If the filesystem cannot flush it, the
        // caller reports publication uncertainty rather than claiming durability.
        OpenOptions::new()
            .read(true)
            .custom_flags(0x0200_0000)
            .open(path)?
            .sync_all()
    }
    #[cfg(not(windows))]
    {
        File::open(path)?.sync_all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn post_publication_sync_failure_retains_exact_target_and_identity() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("node.toml");
        let bytes = b"config_version = 1\n";
        let error = publish(&output, bytes, |_| {
            Err(std::io::Error::other("injected directory sync failure"))
        })
        .unwrap_err();
        let uncertain = error
            .downcast_ref::<MigrationPublicationUncertain>()
            .unwrap();
        assert_eq!(uncertain.output, output);
        assert_eq!(uncertain.output_sha256, hex::encode(Sha256::digest(bytes)));
        assert_eq!(fs::read(&output).unwrap(), bytes);
        assert!(publish(&output, b"replacement", |_| Ok(())).is_err());
        assert_eq!(fs::read(&output).unwrap(), bytes);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }
}
