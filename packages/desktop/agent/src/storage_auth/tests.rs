use super::*;

fn s3(name: &str) -> Connection {
    Connection {
        bucket: "bucket".into(),
        region: "region".into(),
        endpoint: "https://storage.invalid".into(),
        credentials: Credentials::S3 {
            access_key_id: name.into(),
            secret_access_key: "fixture-secret".into(),
            session_token: None,
        },
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_requests_keep_provider_credentials_in_their_own_subprocesses() {
    async fn configured(name: &str) -> String {
        scope(Some(s3(name)), async {
            tokio::task::yield_now().await;
            let mut command = tokio::process::Command::new("git");
            configure_command(&mut command).unwrap();
            command
                .as_std()
                .get_envs()
                .find(|(key, _)| *key == "AWS_ACCESS_KEY_ID")
                .unwrap()
                .1
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned()
        })
        .await
    }
    let values = tokio::join!(configured("fixture-first"), configured("fixture-second"));
    assert_eq!(values, ("fixture-first".into(), "fixture-second".into()));
    assert!(current().is_none());
}

#[test]
fn selected_credentials_cannot_be_used_for_another_bucket_or_provider() {
    for url in [
        "crab://other/repo",
        "gs://bucket/repo",
        "crab://bucket/repo?sig=fixture",
        "crab://user:fixture@bucket/repo",
    ] {
        assert!(s3("fixture").repository_prefix(url).is_err(), "{url}");
    }
    assert_eq!(
        s3("fixture").cli_url("s3://bucket/repo").unwrap(),
        "crab://bucket/repo"
    );
}

#[test]
fn azure_repository_identity_includes_account_and_container() {
    let connection = Connection {
        bucket: "container".into(),
        region: "".into(),
        endpoint: "".into(),
        credentials: Credentials::Azure {
            account: "account".into(),
            access_key: Some("YWJj".into()),
            sas_token: None,
        },
    };
    assert_eq!(
        connection
            .repository_prefix("az://account/container/repo")
            .unwrap(),
        "repo"
    );
    assert_eq!(
        connection.cli_url("az://account/container/repo").unwrap(),
        "crab://container/repo"
    );
    assert!(
        connection
            .repository_prefix("az://account/other/repo")
            .is_err()
    );
}
#[tokio::test]
async fn data_backend_configuration_and_named_git_remotes_keep_distinct_owners() {
    let directory = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(directory.path())
            .output()
            .unwrap()
    };
    assert!(run(&["init"]).status.success());
    assert!(
        run(&[
            "remote",
            "add",
            "origin",
            "https://forge.invalid/project.git"
        ])
        .status
        .success()
    );
    assert!(
        run(&["remote", "add", "archive", "crab://other/repository"])
            .status
            .success()
    );
    tokio::fs::write(
        directory.path().join("crab.toml"),
        "[remote]\nurl = 'crab://bucket/repository'\n",
    )
    .await
    .unwrap();
    assert_eq!(
        remote_url(Some(directory.path()), "", None).await.unwrap(),
        "crab://bucket/repository"
    );
    assert_eq!(
        remote_url(Some(directory.path()), "", Some("origin"))
            .await
            .unwrap(),
        "https://forge.invalid/project.git"
    );
    let state = crate::handlers::State::new();
    scope(Some(s3("fixture")), async {
        let params = json!({ "cwd": directory.path() });
        assert!(
            validate_target(&state, "crab_add_structured", &params)
                .await
                .is_ok()
        );
        assert!(
            validate_target(&state, "git_fetch_all", &params)
                .await
                .is_err()
        );
        let params = json!({ "cwd": directory.path(), "remote": "archive" });
        assert!(
            validate_target(&state, "crab_push_structured", &params)
                .await
                .is_err()
        );
    })
    .await;
}

#[tokio::test]
async fn azure_connection_probe_sends_account_key_or_sas_authentication() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for key_auth in [true, false] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let received = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            while !bytes.ends_with(b"\r\n\r\n") {
                bytes.push(stream.read_u8().await.unwrap());
                assert!(bytes.len() < 16 * 1024);
            }
            let body = "<EnumerationResults><Blobs/><NextMarker/></EnumerationResults>";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8(bytes).unwrap()
        });
        let connection = Connection {
            bucket: "container".into(),
            region: "".into(),
            endpoint,
            credentials: Credentials::Azure {
                account: "account".into(),
                access_key: key_auth.then(|| "YWJj".into()),
                sas_token: (!key_auth).then(|| "sp=rl&sig=fixture%2Bsignature%3D".into()),
            },
        };
        tokio::time::timeout(Duration::from_secs(10), connection.probe())
            .await
            .unwrap()
            .unwrap();
        let headers = received.await.unwrap();
        if key_auth {
            assert!(
                headers
                    .to_ascii_lowercase()
                    .contains("authorization: sharedkey account:")
            );
        } else {
            let target = headers
                .lines()
                .next()
                .unwrap()
                .split_whitespace()
                .nth(1)
                .unwrap();
            let url = url::Url::parse(&format!("http://localhost{target}")).unwrap();
            assert_eq!(
                url.query_pairs().find(|(name, _)| name == "sig").unwrap().1,
                "fixture+signature="
            );
        }
    }
}
