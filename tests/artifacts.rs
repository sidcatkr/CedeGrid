use resource_manager::{artifacts::ArtifactStore, protocol::ArtifactRef};
use sha2::{Digest, Sha256};
fn dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(".artifact-test-")
        .tempdir_in(std::env::current_dir().unwrap())
        .unwrap()
}
fn reference(data: &[u8]) -> ArtifactRef {
    ArtifactRef {
        sha256: hex::encode(Sha256::digest(data)),
        size: data.len() as u64,
    }
}
#[test]
fn upload_restart_idempotence_and_atomic_publication() {
    let d = dir();
    let data = b"checkpoint-content";
    let a = reference(data);
    let store = ArtifactStore::open(&d.path().join("objects"), 1024, 10240).unwrap();
    let (id, offset) = store.begin("attempt", 1, &a).unwrap();
    assert_eq!(offset, 0);
    store.append(&id, 0, &data[..5]).unwrap();
    assert!(store.publish(&id).is_err());
    drop(store);
    let store = ArtifactStore::open(&d.path().join("objects"), 1024, 10240).unwrap();
    assert_eq!(store.begin("attempt", 1, &a).unwrap().1, 5);
    assert_eq!(store.append(&id, 0, &data[..5]).unwrap(), 5);
    assert!(store.append(&id, 0, b"other").is_err());
    store.append(&id, 5, &data[5..]).unwrap();
    store.publish(&id).unwrap();
    store.publish(&id).unwrap();
    store.verify(&a).unwrap();
    assert_eq!(
        store.read(&a.sha256, 0, 1024).unwrap(),
        (data.to_vec(), true)
    );
}
#[test]
fn checksum_quota_and_path_traversal_fail_closed() {
    let d = dir();
    let store = ArtifactStore::open(&d.path().join("objects"), 16, 1000).unwrap();
    assert!(store.begin("a", 1, &reference(&[0; 17])).is_err());
    assert!(store.read("../../secret", 0, 10).is_err());
    let a = reference(b"true");
    let (id, _) = store.begin("a", 1, &a).unwrap();
    store.append(&id, 0, b"fake").unwrap();
    assert!(store.publish(&id).is_err());
    assert!(store.read(&a.sha256, 0, 10).is_err());
}
#[cfg(unix)]
#[test]
fn symlink_blob_is_never_followed() {
    use std::os::unix::fs::symlink;
    let d = dir();
    let root = d.path().join("objects");
    let store = ArtifactStore::open(&root, 1024, 10240).unwrap();
    let a = reference(b"data");
    let (id, _) = store.begin("a", 1, &a).unwrap();
    store.append(&id, 0, b"data").unwrap();
    std::fs::write(d.path().join("unrelated"), b"data").unwrap();
    symlink(
        d.path().join("unrelated"),
        root.join("blobs").join(&a.sha256),
    )
    .unwrap();
    assert!(store.publish(&id).is_err());
    assert!(store.read(&a.sha256, 0, 10).is_err());
}
