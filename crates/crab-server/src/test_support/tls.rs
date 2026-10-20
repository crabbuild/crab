use std::process::Command;

use tempfile::TempDir;
use url::Url;

use crate::CellsConfig;

pub(crate) struct IdentityFiles {
    _directory: TempDir,
    pub(crate) certificate: std::path::PathBuf,
    private_key: std::path::PathBuf,
    pub(crate) other_key: std::path::PathBuf,
    pub(crate) ca: std::path::PathBuf,
}

impl IdentityFiles {
    pub(crate) fn generate() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let ca_key = directory.path().join("ca.key");
        let ca = directory.path().join("ca.crt");
        let private_key = directory.path().join("peer.key");
        let other_key = directory.path().join("other.key");
        let request = directory.path().join("peer.csr");
        let certificate = directory.path().join("peer.crt");
        let extensions = directory.path().join("peer.ext");
        run(Command::new("openssl")
            .args(["genpkey", "-algorithm", "ED25519", "-out"])
            .arg(&ca_key));
        run(Command::new("openssl")
            .args([
                "req",
                "-x509",
                "-new",
                "-days",
                "1",
                "-subj",
                "/CN=Crab Test CA",
                "-addext",
                "basicConstraints=critical,CA:TRUE",
                "-key",
            ])
            .arg(&ca_key)
            .arg("-out")
            .arg(&ca));
        for key in [&private_key, &other_key] {
            run(Command::new("openssl")
                .args(["genpkey", "-algorithm", "ED25519", "-out"])
                .arg(key));
        }
        run(Command::new("openssl")
            .args(["req", "-new", "-subj", "/CN=localhost", "-key"])
            .arg(&private_key)
            .arg("-out")
            .arg(&request));
        std::fs::write(
            &extensions,
            "basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth,clientAuth\nsubjectAltName=DNS:localhost\n",
        )
        .unwrap();
        run(Command::new("openssl")
            .args(["x509", "-req", "-days", "1", "-CAcreateserial", "-in"])
            .arg(&request)
            .arg("-CA")
            .arg(&ca)
            .arg("-CAkey")
            .arg(&ca_key)
            .arg("-extfile")
            .arg(&extensions)
            .arg("-out")
            .arg(&certificate));
        Self {
            _directory: directory,
            certificate,
            private_key,
            other_key,
            ca,
        }
    }

    pub(crate) fn config(&self, endpoint: Url) -> CellsConfig {
        CellsConfig {
            data_dir: self._directory.path().join("cells"),
            local_disk_limit_bytes: 32 * 1024 * 1024 * 1024,
            peer_advertise: endpoint,
            failure_zone: None,
            peer_tls_server_name: None,
            peer_certificate: self.certificate.clone(),
            peer_private_key: self.private_key.clone(),
            peer_ca: self.ca.clone(),
            durability: crate::CellDurabilityMode::Fleet,
        }
    }
}

fn run(command: &mut Command) {
    assert!(command.output().unwrap().status.success());
}
