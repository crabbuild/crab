use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt as _;
use std::process::Command;

use crab_remote::local::{CURRENT_CAPABILITIES_JSON, DIRECT_CDN_CHECKOUT_BINDING_ENV};
use tokio_util::sync::CancellationToken;

use super::{LocalTools, issuer_only_checkout};
use crate::local::{CloneOptions, Options};
use crate::storage::{CdnIssuerOptions, DirectCdnOptions, DirectStoreOptions, S3Options};
use crate::{Client, ErrorKind, RepositoryLocator};

#[test]
fn private_checkout_delegates_only_the_current_binding() {
    const CHILD: &str = "CRAB_SDK_CHECKOUT_ENVIRONMENT_TEST";
    if std::env::var_os(CHILD).is_none() {
        let test = format!(
            "{}::private_checkout_delegates_only_the_current_binding",
            module_path!().split_once("::").unwrap().1
        );
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &test, "--nocapture"])
            .env(CHILD, "1")
            .env(DIRECT_CDN_CHECKOUT_BINDING_ENV, "stale-binding")
            .env("AWS_SESSION_TOKEN", "ambient-session")
            .env("GOOGLE_BEARER_TOKEN", "ambient-google")
            .env("AZURE_STORAGE_TOKEN", "ambient-azure")
            .env("MSI_SECRET", "ambient-identity")
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }

    let store = DirectStoreOptions::s3(
        S3Options::new(
            "fixture-bucket",
            "us-east-1",
            "explicit-key",
            "explicit-secret",
        )
        .unwrap(),
    );
    let owner = LocalTools::new("/bin/sh", "/bin/sh")
        .unwrap()
        .with_store(&store)
        .unwrap()
        .owner;
    let isolated = issuer_only_checkout(&owner);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let cancel = CancellationToken::new();
        let probe = r#"
            test "$CRAB_INTERNAL_SDK_CDN_BINDING_FILE" = "$1" &&
            test -z "${AWS_ACCESS_KEY_ID+x}" &&
            test -z "${AWS_SECRET_ACCESS_KEY+x}" &&
            test -z "${AWS_SESSION_TOKEN+x}" &&
            test -z "${GOOGLE_BEARER_TOKEN+x}" &&
            test -z "${AZURE_STORAGE_TOKEN+x}" &&
            test -z "${MSI_SECRET+x}"
        "#;
        isolated
            .run_git(None, ["-c", probe, "probe", ""], true, &cancel)
            .await
            .unwrap();
        isolated
            .run_git_with_env(
                None,
                ["-c", probe, "probe", "current-binding"],
                [(
                    DIRECT_CDN_CHECKOUT_BINDING_ENV,
                    OsString::from("current-binding"),
                )],
                true,
                &cancel,
            )
            .await
            .unwrap();
        owner
            .run_git(
                None,
                [
                    "-c",
                    r#"test -z "${CRAB_INTERNAL_SDK_CDN_BINDING_FILE+x}" &&
                    test "$AWS_ACCESS_KEY_ID" = explicit-key"#,
                ],
                true,
                &cancel,
            )
            .await
            .unwrap();

        // Clone performs both handshakes before checking the destination.
        // Stop at a collision so the subprocess boundary needs no cloud I/O.
        let scratch = tempfile::tempdir().unwrap();
        let executable = scratch.path().join("checkout-probe");
        std::fs::write(
            &executable,
            format!(
                r#"#!/bin/sh
set -e
( set -- ''; {probe} )
case "$1" in
    --version) printf '%s\n' 'git version 2.45.2' ;;
    sdk-capabilities) printf '%s\n' '{CURRENT_CAPABILITIES_JSON}' ;;
    *) exit 64 ;;
esac
"#
            ),
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let issuer = CdnIssuerOptions::new(
            "https://auth.example.test/v1/credentials",
            "https://identity.example.test",
            "checkout-probe",
            scratch.path().join("oidc-cache"),
        )
        .unwrap();
        let cdn = DirectCdnOptions::private_export(
            "s3://fixture-bucket",
            "team/models",
            "https://edge.example.test",
            issuer,
            None,
        )
        .unwrap();
        let client = Client::builder()
            .direct_store(store)
            .direct_cdn(cdn)
            .local(Options::new(
                LocalTools::new(&executable, &executable).unwrap(),
            ))
            .build()
            .unwrap();
        let result = client
            .clone_local(
                RepositoryLocator::new("team/models").unwrap(),
                scratch.path().to_owned(),
                CloneOptions::default(),
            )
            .await;
        assert_eq!(
            result.err().map(|error| error.kind()),
            Some(ErrorKind::Conflict)
        );
        client.close().await.unwrap();
    });
}
