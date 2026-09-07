use std::fmt::Debug;

use crab_cache::CacheServiceAuth;

const SECRET: &str = "fixture-private-cache-credential";

fn assert_redacted(value: &dyn Debug) {
    for rendered in [format!("{value:?}"), format!("{value:#?}")] {
        assert!(
            !rendered.contains(SECRET),
            "Debug exposed cache credential material"
        );
    }
}

#[test]
fn service_auth_redacts_psk_and_bearer_credentials() {
    for auth in [
        CacheServiceAuth::Psk(SECRET.into()),
        CacheServiceAuth::Bearer(SECRET.into()),
    ] {
        assert_redacted(&auth);
    }
}

#[cfg(feature = "active-probe")]
#[test]
fn active_probe_auth_redacts_borrowed_credentials() {
    for auth in [
        crab_cache::ActiveProbeAuth::Psk(SECRET),
        crab_cache::ActiveProbeAuth::Bearer(SECRET),
    ] {
        assert_redacted(&auth);
    }
}

#[cfg(feature = "remote-client")]
#[test]
fn cache_client_redacts_stored_auth_headers() {
    for auth in [
        CacheServiceAuth::Psk(SECRET.into()),
        CacheServiceAuth::Bearer(SECRET.into()),
    ] {
        let client =
            crab_cache::CacheClient::new("https://cache.example.test", &auth, None, None, None)
                .expect("construct client without network I/O");
        assert_redacted(&client);
    }
}
