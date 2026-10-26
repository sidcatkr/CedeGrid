//! Publishing an assignment's outputs and verifying publication receipts.
use super::*;

pub(super) fn sha256_file(path: &Path) -> Result<(String, u64)> {
    let mut file = crate::publication::open_regular(path, false)?;
    let expected = file.metadata()?.len();
    ensure!(
        expected <= 256 * 1024 * 1024,
        "artifact exceeds maximum supported bytes"
    );
    hash_open_file(&mut file, expected)
}

pub(super) fn hash_open_file(file: &mut File, expected: u64) -> Result<(String, u64)> {
    use sha2::{Digest, Sha256};
    use std::io::{Seek, SeekFrom};
    file.seek(SeekFrom::Start(0))?;
    let mut hash = Sha256::new();
    let mut size = 0u64;
    let mut buffer = [0u8; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        size = size
            .checked_add(n as u64)
            .context("artifact size overflow")?;
        ensure!(size <= expected, "artifact grew while reading");
        hash.update(&buffer[..n]);
    }
    ensure!(size == expected, "artifact changed length while reading");
    Ok((hex::encode(hash.finalize()), size))
}

pub(super) async fn publish(
    client: &NodeClient,
    assignment: &Assignment,
    output: &Path,
    checkpoint: bool,
    descriptor_bytes: Option<Vec<u8>>,
) -> Result<()> {
    let path = output.join(if checkpoint {
        "checkpoint.json"
    } else {
        "result.json"
    });
    let descriptor_hash = {
        use sha2::{Digest, Sha256};
        descriptor_bytes
            .as_ref()
            .map(|b| hex::encode(Sha256::digest(b)))
            .unwrap_or_else(|| "command".into())
    };
    let descriptor: serde_json::Value = if let Some(bytes) = descriptor_bytes.as_deref() {
        crate::numeric::from_slice(bytes)?
    } else {
        ensure!(!checkpoint, "checkpoint descriptor missing");
        ensure!(
            !path.try_exists()?,
            "legacy outbox requires explicit reconciliation; automatic conversion refused"
        );
        let outcome: ExecutionOutcome = serde_json::from_slice(&crate::publication::read_bounded(
            &output.join("execution-outcome.json"),
            2 * 1024 * 1024,
        )?)?;
        ensure!(
            outcome.exit_code == Some(0) && !outcome.yielded,
            "command did not complete normally"
        );
        serde_json::json!({"schema_version":2,"kind":"result","task_id":assignment.request.task_id,"assignment_id":assignment.request.assignment_id,"generation":assignment.generation,"artifacts":[],"metadata":{"exit_code":0}})
    };
    ensure!(
        descriptor["task_id"] == assignment.request.task_id
            && descriptor["assignment_id"] == assignment.request.assignment_id
            && descriptor["generation"] == assignment.generation,
        "worker descriptor attempt mismatch"
    );
    let mut artifacts = Vec::new();
    for item in descriptor["artifacts"]
        .as_array()
        .context("artifact list missing")?
    {
        let relative = Path::new(item["path"].as_str().context("artifact path missing")?);
        ensure!(
            relative
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_))),
            "artifact path must remain within attempt output"
        );
        let path = output.join(relative);
        let mut file = crate::publication::open_regular(&path, false)?;
        let expected = item["size"].as_u64().context("artifact size missing")?;
        ensure!(
            expected <= 256 * 1024 * 1024,
            "artifact exceeds maximum supported bytes"
        );
        let (sha256, size) = hash_open_file(&mut file, expected)?;
        ensure!(
            item["sha256"] == sha256,
            "worker artifact integrity mismatch"
        );
        let artifact = ArtifactRef { sha256, size };
        let Response::Upload {
            upload_id,
            mut offset,
        } = client
            .request(&Request::BeginUpload {
                assignment_id: assignment.request.assignment_id.clone(),
                generation: assignment.generation,
                artifact: artifact.clone(),
            })
            .await?
        else {
            bail!("unexpected upload response")
        };
        use std::io::{Seek, SeekFrom};
        file.seek(SeekFrom::Start(offset))?;
        let mut buf = vec![0; 128 * 1024];
        while offset < size {
            let remaining = usize::try_from(size - offset)?.min(buf.len());
            let n = file.read(&mut buf[..remaining])?;
            ensure!(n > 0, "artifact truncated during upload");
            let response = client
                .request(&Request::UploadChunk {
                    upload_id: upload_id.clone(),
                    offset,
                    data_hex: hex::encode(&buf[..n]),
                })
                .await?;
            let Response::Upload { offset: ack, .. } = response else {
                bail!("unexpected chunk response")
            };
            ensure!(
                ack == offset + n as u64,
                "unexpected upload acknowledgement"
            );
            offset = ack;
        }
        ensure!(
            hash_open_file(&mut file, artifact.size)? == (artifact.sha256.clone(), artifact.size),
            "artifact changed during publication"
        );
        ensure!(
            matches!(
                client.request(&Request::CommitUpload { upload_id }).await?,
                Response::Artifact { .. }
            ),
            "artifact publication failed"
        );
        artifacts.push(artifact);
    }
    let submission = ResultSubmission {
        task_id: assignment.request.task_id.clone(),
        assignment_id: assignment.request.assignment_id.clone(),
        generation: assignment.generation,
        result: descriptor,
        artifacts,
    };
    let published = submission.clone();
    let response = client
        .request(&if checkpoint {
            Request::PublishCheckpoint { submission }
        } else {
            Request::Complete { submission }
        })
        .await?;
    verify_publication_receipt(&response, &published)?;
    let receipt_path = output.join(if checkpoint {
        format!("checkpoint-{descriptor_hash}.receipt.json")
    } else {
        "result.receipt.json".into()
    });
    if !receipt_path.exists() {
        client.write_json_new(
            &receipt_path,
            &serde_json::json!({"response":response,"submission":published}),
        )?;
    }
    if std::env::var_os("CEDEGRID_PUBLICATION_EVIDENCE").as_deref()
        == Some(std::ffi::OsStr::new("1"))
    {
        emit(
            &serde_json::json!({"event":"publication_accepted","checkpoint":checkpoint,"submission":published,"receipt":response}),
        )?;
    }
    Ok(())
}

pub(crate) fn verify_publication_receipt(
    response: &Response,
    submission: &ResultSubmission,
) -> Result<()> {
    use sha2::{Digest, Sha256};
    let Response::Receipt { receipt } = response else {
        bail!("authoritative result acceptance missing durable commit receipt")
    };
    let expected = hex::encode(Sha256::digest(serde_json::to_vec(submission)?));
    ensure!(
        receipt.task_id == submission.task_id
            && receipt.assignment_id == submission.assignment_id
            && receipt.generation == submission.generation
            && receipt.receipt_hash == expected,
        "authoritative publication receipt identity or content mismatch"
    );
    Ok(())
}
