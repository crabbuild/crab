use std::collections::BTreeSet;
use std::net::IpAddr;
use std::time::Duration;

use serde::Deserialize;

const MAX_METADATA_BYTES: usize = 64 * 1024;
const INVALID_PEER_ADDRESS: &str = "ECS metadata must contain exactly one usable awsvpc IPv4 address, or one IPv6 address when IPv4 is absent";

#[derive(Deserialize)]
struct ContainerMetadata {
    #[serde(rename = "Networks", default)]
    networks: Vec<Network>,
}

#[derive(Deserialize)]
struct Network {
    #[serde(rename = "NetworkMode", default)]
    mode: String,
    #[serde(rename = "IPv4Addresses", default)]
    ipv4_addresses: Vec<String>,
    #[serde(rename = "IPv6Addresses", default)]
    ipv6_addresses: Vec<String>,
}

pub(crate) async fn peer_advertise_host() -> crab_http_server::Result<String> {
    let endpoint = std::env::var("ECS_CONTAINER_METADATA_URI_V4").map_err(|_| {
        crab_http_server::Error::Config(
            "--peer-advertise-host-from-ecs-metadata requires ECS_CONTAINER_METADATA_URI_V4",
        )
    })?;
    let endpoint =
        url::Url::parse(&endpoint).map_err(|source| crab_http_server::Error::PeerDiscovery {
            context: "ECS_CONTAINER_METADATA_URI_V4 is not a URL",
            source: Box::new(source),
        })?;
    validate_metadata_endpoint(&endpoint)?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(1))
        .timeout(Duration::from_secs(3))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .map_err(|source| crab_http_server::Error::PeerDiscovery {
            context: "ECS metadata client initialization failed",
            source: Box::new(source),
        })?;
    let mut response = client
        .get(endpoint)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|source| crab_http_server::Error::PeerDiscovery {
            context: "ECS container metadata request failed",
            source: Box::new(source),
        })?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_METADATA_BYTES as u64)
    {
        return Err(crab_http_server::Error::Config(
            "ECS container metadata exceeds the byte limit",
        ));
    }
    let mut body = Vec::new();
    while let Some(chunk) =
        response
            .chunk()
            .await
            .map_err(|source| crab_http_server::Error::PeerDiscovery {
                context: "ECS container metadata body failed",
                source: Box::new(source),
            })?
    {
        if body.len().saturating_add(chunk.len()) > MAX_METADATA_BYTES {
            return Err(crab_http_server::Error::Config(
                "ECS container metadata exceeds the byte limit",
            ));
        }
        body.extend_from_slice(&chunk);
    }
    peer_advertise_host_from_bytes(&body)
}

fn validate_metadata_endpoint(endpoint: &url::Url) -> crab_http_server::Result<()> {
    let expected = std::net::Ipv4Addr::new(169, 254, 170, 2);
    if endpoint.scheme() != "http"
        || endpoint.host() != Some(url::Host::Ipv4(expected))
        || endpoint.port_or_known_default() != Some(80)
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
        || !endpoint.path().starts_with("/v4/")
    {
        return Err(crab_http_server::Error::Config(
            "ECS_CONTAINER_METADATA_URI_V4 must use the task-local metadata endpoint",
        ));
    }
    Ok(())
}

fn peer_advertise_host_from_bytes(bytes: &[u8]) -> crab_http_server::Result<String> {
    let metadata: ContainerMetadata =
        serde_json::from_slice(bytes).map_err(|source| crab_http_server::Error::PeerDiscovery {
            context: "ECS container metadata is invalid",
            source: Box::new(source),
        })?;
    let mut ipv4 = BTreeSet::new();
    let mut ipv6 = BTreeSet::new();
    for network in metadata
        .networks
        .iter()
        .filter(|network| network.mode == "awsvpc")
    {
        collect_peer_addresses(&network.ipv4_addresses, false, &mut ipv4);
        collect_peer_addresses(&network.ipv6_addresses, true, &mut ipv6);
    }
    if ipv4.len() == 1 {
        return ipv4
            .pop_first()
            .ok_or(crab_http_server::Error::Config(INVALID_PEER_ADDRESS));
    }
    if ipv4.is_empty() && ipv6.len() == 1 {
        return ipv6
            .pop_first()
            .ok_or(crab_http_server::Error::Config(INVALID_PEER_ADDRESS));
    }
    Err(crab_http_server::Error::Config(INVALID_PEER_ADDRESS))
}

fn collect_peer_addresses(values: &[String], ipv6: bool, output: &mut BTreeSet<String>) {
    for value in values {
        let Ok(address) = value.parse::<IpAddr>() else {
            continue;
        };
        let link_local_or_broadcast = match address {
            IpAddr::V4(address) => address.is_link_local() || address.is_broadcast(),
            IpAddr::V6(address) => address.is_unicast_link_local(),
        };
        if address.is_unspecified()
            || address.is_loopback()
            || address.is_multicast()
            || link_local_or_broadcast
            || ipv6 != address.is_ipv6()
        {
            continue;
        }
        output.insert(address.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_requires_the_task_local_endpoint_and_one_awsvpc_address() {
        validate_metadata_endpoint(
            &"http://169.254.170.2/v4/01234567-89ab-cdef-0123-456789abcdef"
                .parse()
                .unwrap(),
        )
        .unwrap();
        for endpoint in [
            "https://169.254.170.2/v4/task",
            "http://metadata.example/v4/task",
            "http://169.254.170.2/task",
            "http://169.254.170.2/v4/task?token=secret",
        ] {
            assert!(
                validate_metadata_endpoint(&endpoint.parse().unwrap()).is_err(),
                "{endpoint}"
            );
        }

        let address = peer_advertise_host_from_bytes(
            br#"{
              "Networks": [{
                "NetworkMode": "awsvpc",
                "IPv4Addresses": ["10.42.3.17"],
                "IPv6Addresses": []
              }],
              "FutureField": true
            }"#,
        )
        .unwrap();
        assert_eq!(address, "10.42.3.17");

        let address = peer_advertise_host_from_bytes(
            br#"{"Networks":[{"NetworkMode":"awsvpc","IPv6Addresses":["2001:db8::17"]}]}"#,
        )
        .unwrap();
        assert_eq!(address, "2001:db8::17");

        for metadata in [
            br#"{"Networks": []}"#.as_slice(),
            br#"{"Networks": [{"NetworkMode":"bridge","IPv4Addresses":["10.0.0.1"]}]}"#,
            br#"{"Networks": [{"NetworkMode":"awsvpc","IPv4Addresses":["10.0.0.1","10.0.0.2"]}]}"#,
            br#"{"Networks": [{"NetworkMode":"awsvpc","IPv4Addresses":["127.0.0.1"]}]}"#,
            br#"{"Networks": [{"NetworkMode":"awsvpc","IPv4Addresses":["169.254.1.1"]}]}"#,
        ] {
            assert!(peer_advertise_host_from_bytes(metadata).is_err());
        }
    }
}
