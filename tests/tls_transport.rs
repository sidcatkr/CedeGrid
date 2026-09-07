//! Real loopback TLS, private ephemeral CA, and certificate-bound RPC authorization.
use resource_manager::{coordinator::serve, protocol::*};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::Path, process::Command};
fn openssl(dir: &Path, args: &[&str]) -> Vec<u8> {
    let output = Command::new("/usr/bin/openssl")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("openssl required for native TLS integration test");
    assert!(
        output.status.success(),
        "openssl failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}
fn identity(dir: &Path, name: &str, server: bool) -> TlsIdentity {
    openssl(
        dir,
        &[
            "req",
            "-new",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-subj",
            &format!("/CN={name}"),
            "-keyout",
            &format!("{name}.key"),
            "-out",
            &format!("{name}.csr"),
        ],
    );
    std::fs::write(dir.join(format!("{name}.ext")),if server{"basicConstraints=CA:FALSE\nkeyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\nsubjectAltName=IP:127.0.0.1\n"}else{"basicConstraints=CA:FALSE\nkeyUsage=digitalSignature\nextendedKeyUsage=clientAuth\n"}).unwrap();
    openssl(
        dir,
        &[
            "x509",
            "-req",
            "-in",
            &format!("{name}.csr"),
            "-CA",
            "ca.pem",
            "-CAkey",
            "ca.key",
            "-CAcreateserial",
            "-days",
            "1",
            "-extfile",
            &format!("{name}.ext"),
            "-out",
            &format!("{name}.pem"),
        ],
    );
    TlsIdentity {
        ca_cert: dir.join("ca.pem"),
        certificate: dir.join(format!("{name}.pem")),
        private_key: dir.join(format!("{name}.key")),
    }
}
fn fingerprint(dir: &Path, name: &str) -> String {
    hex::encode(Sha256::digest(openssl(
        dir,
        &["x509", "-in", &format!("{name}.pem"), "-outform", "DER"],
    )))
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_mtls_rejects_missing_unlisted_and_wrong_role_certificates() {
    let directory = tempfile::Builder::new()
        .prefix(".tls-test-")
        .tempdir_in(std::env::current_dir().unwrap())
        .unwrap();
    let dir = directory.path();
    openssl(
        dir,
        &[
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-days",
            "1",
            "-subj",
            "/CN=ResourceManager-Test-CA",
            "-keyout",
            "ca.key",
            "-out",
            "ca.pem",
        ],
    );
    let server = identity(dir, "server", true);
    let operator = identity(dir, "operator", false);
    let node = identity(dir, "node", false);
    let outsider = identity(dir, "outsider", false);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let config = CoordinatorConfig {
        storage_profile: Default::default(),
        state_dir: dir.join("state"),
        listen: address,
        tls: server,
        clients: BTreeMap::from([
            (fingerprint(dir, "operator"), Principal::Operator),
            (
                fingerprint(dir, "node"),
                Principal::Node {
                    node_id: "node".into(),
                },
            ),
        ]),
        lease_ms: 10_000,
        telemetry_ttl_ms: 3000,
        max_artifact_bytes: 1024 * 1024,
        artifact_quota_bytes: 10 * 1024 * 1024,
        retry_limit: 3,
        retry_backoff_ms: 1000,
        retry_backoff_max_ms: 30000,
        yield_retry_backoff_ms: 1000,
        yield_retry_backoff_max_ms: 30000,
    };
    let server = tokio::spawn(serve(config));
    let endpoint = format!("https://{address}");
    let client = RpcClient::new(&endpoint, &operator).unwrap();
    let mut ready = false;
    for _ in 0..50 {
        if client
            .request(&Request::Status { job_id: None })
            .await
            .is_ok()
        {
            ready = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(ready, "TLS server failed startup");
    let python = std::env::var("RESMGR_TEST_PYTHON").unwrap_or_else(|_| "python3".into());
    let sdk = Command::new(python)
        .arg("-c")
        .arg(include_str!("test_sdk_client_transport.py"))
        .arg(&endpoint)
        .arg(dir)
        .env(
            "PYTHONPATH",
            std::env::current_dir().unwrap().join("python"),
        )
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("TMPDIR", dir)
        .output()
        .expect("Python SDK interpreter");
    assert!(
        sdk.status.success(),
        "real Python SDK mTLS failed: {}",
        String::from_utf8_lossy(&sdk.stderr)
    );
    assert!(String::from_utf8_lossy(&sdk.stdout).contains("sdk-mtls-pass"));
    assert!(String::from_utf8_lossy(&sdk.stdout).contains("sdk-artifact-roundtrip-pass"));
    let nodeclient = RpcClient::new(&endpoint, &node).unwrap();
    assert!(
        nodeclient
            .request(&Request::Status { job_id: None })
            .await
            .is_err()
    );
    let outsiderclient = RpcClient::new(&endpoint, &outsider).unwrap();
    assert!(
        outsiderclient
            .request(&Request::Status { job_id: None })
            .await
            .is_err()
    );
    let ca = reqwest::Certificate::from_pem(&std::fs::read(dir.join("ca.pem")).unwrap()).unwrap();
    let nocert = reqwest::Client::builder()
        .tls_built_in_root_certs(false)
        .add_root_certificate(ca)
        .build()
        .unwrap();
    assert!(
        nocert
            .post(format!("{endpoint}/v1/rpc"))
            .json(&Request::Status { job_id: None })
            .send()
            .await
            .is_err()
    );
    let plain = reqwest::Client::new()
        .post(format!("http://{address}/v1/rpc"))
        .json(&Request::Status { job_id: None })
        .send()
        .await;
    assert!(plain.is_err());
    assert!(RpcClient::new(&format!("http://{address}"), &operator).is_err());
    let wrong_hostname =
        RpcClient::new(&format!("https://localhost:{}", address.port()), &operator).unwrap();
    assert!(
        wrong_hostname
            .request(&Request::Status { job_id: None })
            .await
            .is_err()
    );
    drop((client, nodeclient, outsiderclient, nocert, wrong_hostname));
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}
