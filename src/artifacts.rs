//! Immutable bounded artifact publication. Database publication follows directory sync.
use crate::protocol::ArtifactRef;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};
pub const CHUNK_LIMIT: usize = 1024 * 1024;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadMetadata {
    pub assignment_id: String,
    pub generation: u64,
    pub artifact: ArtifactRef,
}
pub struct ArtifactStore {
    root: PathBuf,
    max_bytes: u64,
    quota_bytes: u64,
}
impl ArtifactStore {
    pub fn open(root: &Path, max_bytes: u64, quota_bytes: u64) -> Result<Self> {
        ensure!(
            max_bytes > 0 && quota_bytes >= max_bytes,
            "invalid artifact limits"
        );
        ensure!(
            crate::state::preflight(root)?.supported,
            "artifact storage requires supported local filesystem"
        );
        private_dir(root)?;
        private_dir(&root.join("uploads"))?;
        private_dir(&root.join("blobs"))?;
        Ok(Self {
            root: root.canonicalize()?,
            max_bytes,
            quota_bytes,
        })
    }
    pub fn begin(
        &self,
        assignment_id: &str,
        generation: u64,
        artifact: &ArtifactRef,
    ) -> Result<(String, u64)> {
        validate_hash(&artifact.sha256)?;
        ensure!(
            artifact.size <= self.max_bytes,
            "artifact exceeds configured maximum"
        );
        let metadata = UploadMetadata {
            assignment_id: assignment_id.to_owned(),
            generation,
            artifact: artifact.clone(),
        };
        let data = serde_json::to_vec(&metadata)?;
        let id = hex::encode(Sha256::digest(&data));
        let meta = self.root.join("uploads").join(format!("{id}.json"));
        if meta.exists() {
            regular_metadata(&meta)?;
            ensure!(fs::read(&meta)? == data, "upload metadata collision");
        } else {
            let used = self.disk_usage()?;
            ensure!(
                used.checked_add(
                    artifact
                        .size
                        .checked_mul(2)
                        .context("artifact size overflow")?
                )
                .and_then(|n| n.checked_add(data.len() as u64))
                .context("disk accounting overflow")?
                    <= self.quota_bytes,
                "artifact quota exhausted"
            );
            let temporary = self
                .root
                .join("uploads")
                .join(format!(".metadata-{}.tmp", uuid::Uuid::new_v4()));
            let mut f = create_new(&temporary)?;
            f.write_all(&data)?;
            f.sync_all()?;
            match fs::hard_link(&temporary, &meta) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    regular_metadata(&meta)?;
                    ensure!(fs::read(&meta)? == data, "upload metadata collision");
                }
                Err(e) => return Err(e).context("atomic upload metadata publication"),
            }
            sync_parent(&meta)?;
            fs::remove_file(&temporary)?;
            sync_parent(&temporary)?;
        }
        let partial = self.partial(&id)?;
        if !partial.exists() {
            let f = create_new(&partial)?;
            f.sync_all()?;
            sync_parent(&partial)?;
        }
        let offset = regular_metadata(&partial)?.len();
        ensure!(offset <= artifact.size, "oversized partial artifact");
        Ok((id, offset))
    }
    pub fn metadata(&self, id: &str) -> Result<UploadMetadata> {
        validate_hash(id)?;
        let p = self.root.join("uploads").join(format!("{id}.json"));
        regular_metadata(&p)?;
        Ok(serde_json::from_slice(&fs::read(p)?)?)
    }
    pub fn append(&self, id: &str, offset: u64, data: &[u8]) -> Result<u64> {
        ensure!(data.len() <= CHUNK_LIMIT, "upload chunk too large");
        let meta = self.metadata(id)?;
        let p = self.partial(id)?;
        let len = regular_metadata(&p)?.len();
        ensure!(offset <= len, "upload offset gap");
        ensure!(
            offset
                .checked_add(data.len() as u64)
                .context("offset overflow")?
                <= meta.artifact.size,
            "upload exceeds declared size"
        );
        let mut file = open_regular(&p, true)?;
        if offset < len {
            ensure!(
                offset + data.len() as u64 <= len,
                "overlapping upload retry"
            );
            let mut existing = vec![0; data.len()];
            file.seek(SeekFrom::Start(offset))?;
            file.read_exact(&mut existing)?;
            ensure!(existing == data, "upload retry has different bytes");
            return Ok(len);
        }
        file.seek(SeekFrom::End(0))?;
        file.write_all(data)?;
        file.sync_all()?;
        Ok(offset + data.len() as u64)
    }
    /// Returns only after the blob's inode and final directory entry are durable.
    pub fn publish(&self, id: &str) -> Result<UploadMetadata> {
        let meta = self.metadata(id)?;
        let partial = self.partial(id)?;
        let mut file = open_regular(&partial, false)?;
        ensure!(
            file.metadata()?.len() == meta.artifact.size,
            "incomplete upload"
        );
        let mut digest = Sha256::new();
        let mut buf = [0u8; 65536];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            digest.update(&buf[..n]);
        }
        ensure!(
            hex::encode(digest.finalize()) == meta.artifact.sha256,
            "artifact checksum mismatch"
        );
        file.sync_all()?;
        let final_path = self.blob(&meta.artifact.sha256)?;
        match fs::hard_link(&partial, &final_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                self.verify(&meta.artifact)?;
            }
            Err(e) => return Err(e).context("atomic immutable artifact publication failed"),
        }
        sync_parent(&final_path)?;
        Ok(meta)
    }
    pub fn abort(&self, id: &str) -> Result<()> {
        self.metadata(id)?;
        let partial = self.partial(id)?;
        if partial.exists() {
            regular_metadata(&partial)?;
            fs::remove_file(&partial)?;
        }
        let metadata = self.root.join("uploads").join(format!("{id}.json"));
        fs::remove_file(&metadata)?;
        sync_parent(&metadata)?;
        Ok(())
    }
    pub fn verify(&self, artifact: &ArtifactRef) -> Result<()> {
        let mut file = open_regular(&self.blob(&artifact.sha256)?, false)?;
        ensure!(
            file.metadata()?.len() == artifact.size,
            "published artifact size mismatch"
        );
        let mut d = Sha256::new();
        std::io::copy(&mut file, &mut DigestWriter(&mut d))?;
        ensure!(
            hex::encode(d.finalize()) == artifact.sha256,
            "published artifact digest mismatch"
        );
        Ok(())
    }
    pub fn read(&self, sha256: &str, offset: u64, max_bytes: u32) -> Result<(Vec<u8>, bool)> {
        ensure!(
            max_bytes > 0 && max_bytes as usize <= CHUNK_LIMIT,
            "invalid download chunk size"
        );
        let mut f = open_regular(&self.blob(sha256)?, false)?;
        let size = f.metadata()?.len();
        ensure!(offset <= size, "download offset beyond end");
        f.seek(SeekFrom::Start(offset))?;
        let mut data = vec![0; (size - offset).min(max_bytes as u64) as usize];
        f.read_exact(&mut data)?;
        let eof = offset + data.len() as u64 == size;
        Ok((data, eof))
    }
    pub fn disk_usage(&self) -> Result<u64> {
        let mut total = 0u64;
        for directory in ["uploads", "blobs"] {
            for entry in fs::read_dir(self.root.join(directory))? {
                let entry = entry?;
                let m = regular_metadata(&entry.path())?;
                let charge = if directory == "uploads" {
                    if entry.path().extension().and_then(|s| s.to_str()) == Some("part") {
                        continue;
                    }
                    if entry.path().extension().and_then(|s| s.to_str()) != Some("json") {
                        total = total
                            .checked_add(m.len())
                            .context("artifact quota overflow")?;
                        continue;
                    }
                    let metadata: UploadMetadata =
                        serde_json::from_slice(&fs::read(entry.path())?)?;
                    metadata
                        .artifact
                        .size
                        .saturating_mul(if self.blob(&metadata.artifact.sha256)?.exists() {
                            1
                        } else {
                            2
                        })
                        .saturating_add(m.len())
                } else {
                    m.len()
                };
                total = total
                    .checked_add(charge)
                    .context("artifact quota overflow")?;
            }
        }
        Ok(total)
    }
    fn blob(&self, sha256: &str) -> Result<PathBuf> {
        validate_hash(sha256)?;
        Ok(self.root.join("blobs").join(sha256))
    }
    fn partial(&self, id: &str) -> Result<PathBuf> {
        validate_hash(id)?;
        Ok(self.root.join("uploads").join(format!("{id}.part")))
    }
}
struct DigestWriter<'a>(&'a mut Sha256);
impl Write for DigestWriter<'_> {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.update(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
pub fn validate_hash(hash: &str) -> Result<()> {
    ensure!(
        hash.len() == 64
            && hash
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
        "expected lowercase SHA256"
    );
    Ok(())
}
fn regular_metadata(p: &Path) -> Result<fs::Metadata> {
    let m = fs::symlink_metadata(p)?;
    ensure!(
        m.file_type().is_file(),
        "artifact must be regular, not a link: {}",
        p.display()
    );
    Ok(m)
}
fn open_regular(p: &Path, write: bool) -> Result<File> {
    regular_metadata(p)?;
    let mut o = OpenOptions::new();
    o.read(true).write(write);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(libc::O_NOFOLLOW);
    }
    Ok(o.open(p)?)
}
fn create_new(p: &Path) -> Result<File> {
    let mut o = OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    Ok(o.open(p)?)
}
fn private_dir(p: &Path) -> Result<()> {
    if !p.exists() {
        fs::create_dir_all(p)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(p, fs::Permissions::from_mode(0o700))?;
        }
        sync_parent(p)?;
    }
    ensure!(
        fs::symlink_metadata(p)?.file_type().is_dir(),
        "artifact directory cannot be a symlink"
    );
    File::open(p)?.sync_all()?;
    Ok(())
}
fn sync_parent(p: &Path) -> Result<()> {
    File::open(p.parent().context("artifact path without parent")?)?.sync_all()?;
    Ok(())
}
