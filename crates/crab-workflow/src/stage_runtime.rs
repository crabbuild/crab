//! Core stage definitions: [`Stage`], [`StageName`], [`Cmd`], [`Dep`],
//! [`Out`], [`OutKind`], [`EnvSpec`], [`RetryPolicy`].
//!
//! These types are the in-memory representation of a single workflow
//! stage. They are consumed by the hasher, executor, and journal, and
//! round-tripped through the lockfile via `serde`.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::Dep;
use crate::{Result, WorkflowError as CrabError};
use futures_util::StreamExt;
use object_store::ObjectStoreExt;
use object_store::path::Path as ObjectPath;

/// Resolves and content-hashes URL dependencies for stage execution.
pub trait DepUrlHashExt {
    /// Hashes a URL dependency without named remote aliases.
    #[cfg(any(test, feature = "testing"))]
    fn url_hash(&self) -> Result<Option<(String, [u8; 32])>> {
        self.url_hash_with_remote_aliases(&BTreeMap::new())
    }

    /// Hashes a URL dependency after expanding any configured remote alias.
    fn url_hash_with_remote_aliases(
        &self,
        remote_aliases: &BTreeMap<String, String>,
    ) -> Result<Option<(String, [u8; 32])>>;
}

impl DepUrlHashExt for Dep {
    fn url_hash_with_remote_aliases(
        &self,
        remote_aliases: &BTreeMap<String, String>,
    ) -> Result<Option<(String, [u8; 32])>> {
        let Dep::Url { url, digest } = self else {
            return Ok(None);
        };
        let digest = match digest {
            Some(digest) => parse_pinned_url_digest(url, digest)?,
            None => {
                let expanded = expand_remote_alias_url(url, remote_aliases, "url dep")?;
                hash_live_url_dep(expanded.as_ref())?
            }
        };
        Ok(Some((url.clone(), digest)))
    }
}

fn expand_remote_alias_url<'a>(
    url: &'a str,
    remote_aliases: &'a BTreeMap<String, String>,
    subject: &str,
) -> Result<Cow<'a, str>> {
    let Some((scheme, _)) = url.split_once("://") else {
        return Ok(Cow::Borrowed(url));
    };
    if !scheme.eq_ignore_ascii_case("remote") {
        return Ok(Cow::Borrowed(url));
    }

    let parsed = url::Url::parse(url).map_err(|source| CrabError::Configuration {
        key: format!("{subject} '{url}'"),
        origin: format!("remote alias URL must be a valid remote:// URL: {source}"),
    })?;
    let Some(name) = parsed.host_str().filter(|name| !name.is_empty()) else {
        return Err(CrabError::Configuration {
            key: format!("{subject} '{url}'"),
            origin: "remote alias URL must be remote://<name>/<path>".to_owned(),
        });
    };
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.port().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(CrabError::Configuration {
            key: format!("{subject} '{url}'"),
            origin: "remote alias URL may only contain a remote name and path".to_owned(),
        });
    }

    let Some(base) = remote_aliases.get(name).map(String::as_str) else {
        return Err(CrabError::Configuration {
            key: format!("workflow.remotes.{name}"),
            origin: format!("remote alias URL '{url}' has no matching [workflow.remotes.{name}]"),
        });
    };
    let rel = parsed.path().trim_start_matches('/');
    let expanded = join_remote_alias_base(name, base, rel)?;
    Ok(Cow::Owned(expanded))
}

/// Expands a DVC-style `remote://name/path` output alias.
pub fn expand_external_url_out_alias(
    url: &str,
    remote_aliases: &BTreeMap<String, String>,
) -> Result<String> {
    Ok(expand_remote_alias_url(url, remote_aliases, "external output URL")?.into_owned())
}

fn join_remote_alias_base(name: &str, base: &str, rel: &str) -> Result<String> {
    let trimmed = base.trim();
    if trimmed.is_empty() {
        return Err(CrabError::Configuration {
            key: format!("workflow.remotes.{name}.url"),
            origin: "workflow remote URL must not be empty".to_owned(),
        });
    }

    if let Ok(parsed) = url::Url::parse(trimmed) {
        if parsed.query().is_some() || parsed.fragment().is_some() {
            return Err(CrabError::Configuration {
                key: format!("workflow.remotes.{name}.url"),
                origin: "workflow remote alias base URL must not contain query or fragment"
                    .to_owned(),
            });
        }
        let mut base_url = parsed.to_string();
        if !base_url.ends_with('/') {
            base_url.push('/');
        }
        let base_url = url::Url::parse(&base_url).map_err(|source| CrabError::Configuration {
            key: format!("workflow.remotes.{name}.url"),
            origin: format!("workflow remote alias base URL is invalid: {source}"),
        })?;
        return base_url
            .join(rel)
            .map(|joined| joined.to_string())
            .map_err(|source| CrabError::Configuration {
                key: format!("workflow.remotes.{name}.url"),
                origin: format!("workflow remote alias path is invalid: {source}"),
            });
    }

    let base_path = Path::new(trimmed);
    if base_path.is_absolute() {
        let joined = if rel.is_empty() {
            base_path.to_path_buf()
        } else {
            base_path.join(rel)
        };
        return url::Url::from_file_path(&joined)
            .map(|url| url.to_string())
            .map_err(|()| CrabError::Configuration {
                key: format!("workflow.remotes.{name}.url"),
                origin: "workflow remote alias local path cannot be represented as file://"
                    .to_owned(),
            });
    }

    Err(CrabError::Configuration {
        key: format!("workflow.remotes.{name}.url"),
        origin: "workflow remote alias URL must be an absolute URL or absolute local path"
            .to_owned(),
    })
}

fn parse_pinned_url_digest(url: &str, digest: &str) -> Result<[u8; 32]> {
    let hex = digest
        .strip_prefix("b3:")
        .ok_or_else(|| CrabError::Configuration {
            key: format!("url dep '{url}' digest"),
            origin: "pinned URL deps currently require a b3:<64-hex> digest".to_owned(),
        })?;
    if hex.len() != 64 {
        return Err(CrabError::Configuration {
            key: format!("url dep '{url}' digest"),
            origin: format!(
                "b3 digest must contain 64 hex characters, got {}",
                hex.len()
            ),
        });
    }

    let mut out = [0u8; 32];
    for (index, byte) in out.iter_mut().enumerate() {
        let pair = &hex[index * 2..index * 2 + 2];
        *byte = u8::from_str_radix(pair, 16).map_err(|_| CrabError::Configuration {
            key: format!("url dep '{url}' digest"),
            origin: "b3 digest contains non-hex characters".to_owned(),
        })?;
    }
    Ok(out)
}

fn hash_live_url_dep(url: &str) -> Result<[u8; 32]> {
    let parsed = url::Url::parse(url).map_err(|source| CrabError::Configuration {
        key: format!("url dep '{url}'"),
        origin: format!("URL dependency must be an absolute URL: {source}"),
    })?;
    match parsed.scheme() {
        "http" | "https" => hash_http_url_dep(url),
        "file" => hash_file_url_dep(url, &parsed),
        "s3" | "s3a" | "gs" | "az" | "azure" | "abfs" | "abfss" | "adl" => {
            hash_object_store_url_dep(url)
        }
        _ => Err(CrabError::StageRemoteExecutionUnsupported),
    }
}

fn hash_http_url_dep(url: &str) -> Result<[u8; 32]> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(300))
        .user_agent(format!("crab/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(url_dep_network_error)?;
    let mut response = client.get(url).send().map_err(url_dep_network_error)?;
    let status = response.status();
    if !status.is_success() {
        return Err(CrabError::Storage(object_store::Error::Generic {
            store: "workflow URL dep",
            source: Box::new(std::io::Error::other(format!(
                "GET {url} failed with HTTP {status}"
            ))),
        }));
    }

    let mut hasher = blake3::Hasher::new();
    std::io::copy(&mut response, &mut hasher).map_err(CrabError::Io)?;
    Ok(*hasher.finalize().as_bytes())
}

fn hash_file_url_dep(url: &str, parsed: &url::Url) -> Result<[u8; 32]> {
    let path = parsed
        .to_file_path()
        .map_err(|()| CrabError::Configuration {
            key: format!("url dep '{url}'"),
            origin: "file:// dependency must resolve to a local filesystem path".to_owned(),
        })?;
    let meta = std::fs::metadata(&path).map_err(CrabError::Io)?;
    if meta.is_dir() {
        return Ok(crate::hasher::hash_directory(&path, true)?.hash);
    }
    if !meta.is_file() {
        return Err(CrabError::Configuration {
            key: format!("url dep '{url}'"),
            origin: "file:// dependency must point to a regular file or directory".to_owned(),
        });
    }

    let mut hasher = blake3::Hasher::new();
    let mut file = std::fs::File::open(&path).map_err(CrabError::Io)?;
    std::io::copy(&mut file, &mut hasher).map_err(CrabError::Io)?;
    Ok(*hasher.finalize().as_bytes())
}

fn url_dep_network_error(source: reqwest::Error) -> CrabError {
    CrabError::NetworkTransient(object_store::Error::Generic {
        store: "workflow URL dep",
        source: Box::new(source),
    })
}

fn hash_object_store_url_dep(url: &str) -> Result<[u8; 32]> {
    block_on_url_dep_hash(hash_object_store_url_dep_async(url.to_owned()))
}

fn block_on_url_dep_hash<F>(future: F) -> Result<[u8; 32]>
where
    F: Future<Output = Result<[u8; 32]>> + Send + 'static,
{
    let run = move || -> Result<[u8; 32]> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(CrabError::Io)?;
        runtime.block_on(future)
    };

    if tokio::runtime::Handle::try_current().is_ok() {
        return std::thread::spawn(run).join().map_err(|_| {
            CrabError::Internal("workflow URL dep hash worker panicked".to_owned())
        })?;
    }

    run()
}

async fn hash_object_store_url_dep_async(url: String) -> Result<[u8; 32]> {
    let parsed = url::Url::parse(&url).map_err(|source| CrabError::Configuration {
        key: format!("url dep '{url}'"),
        origin: format!("URL dependency must be an absolute URL: {source}"),
    })?;
    let options: Vec<(String, String)> = std::env::vars()
        .map(|(key, value)| (key.to_ascii_lowercase(), value))
        .collect();
    let (store, location) = object_store::parse_url_opts(
        &parsed,
        options
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str())),
    )
    .map_err(CrabError::Storage)?;
    hash_object_store_location(store.as_ref(), &location).await
}

async fn hash_object_store_location(
    store: &dyn object_store::ObjectStore,
    location: &ObjectPath,
) -> Result<[u8; 32]> {
    match store.get(location).await {
        Ok(result) => {
            let bytes = result.bytes().await.map_err(CrabError::Storage)?;
            return Ok(*blake3::hash(&bytes).as_bytes());
        }
        Err(err) if is_object_store_not_found(&err) => {}
        Err(err) => return Err(CrabError::Storage(err)),
    }

    let mut stream = store.list(Some(location));
    let root_prefix = location.as_ref().trim_end_matches('/');
    let root_child_prefix = if root_prefix.is_empty() {
        String::new()
    } else {
        format!("{root_prefix}/")
    };
    let mut entries = Vec::new();

    while let Some(item) = stream.next().await {
        let meta = item.map_err(CrabError::Storage)?;
        let key = meta.location.as_ref();
        let rel = if root_prefix.is_empty() {
            key
        } else if let Some(rel) = key.strip_prefix(&root_child_prefix) {
            rel
        } else {
            continue;
        };
        if rel.is_empty() || rel.ends_with('/') {
            continue;
        }

        let bytes = store
            .get(&meta.location)
            .await
            .map_err(CrabError::Storage)?
            .bytes()
            .await
            .map_err(CrabError::Storage)?;
        entries.push(crate::hasher::TreeEntry {
            path: PathBuf::from(rel),
            kind: crate::hasher::TreeEntryKind::File,
            file_hash: *blake3::hash(&bytes).as_bytes(),
            size: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            mode: 0o644,
        });
    }

    if entries.is_empty() {
        return Err(CrabError::Storage(object_store::Error::NotFound {
            path: location.to_string(),
            source: Box::new(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "external URL dependency not found",
            )),
        }));
    }

    Ok(crate::hasher::hash_tree_entries(&entries))
}

fn is_object_store_not_found(err: &object_store::Error) -> bool {
    matches!(err, object_store::Error::NotFound { .. })
}

#[cfg(any(test, feature = "testing"))]
pub mod test_support {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// Serves one HTTP response containing `body`.
    pub fn serve_http_body_once(body: &'static [u8]) -> String {
        serve_http_body_n(body, 1)
    }

    /// Serves `requests` HTTP responses containing `body`.
    pub fn serve_http_body_n(body: &'static [u8], requests: usize) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for _ in 0..requests {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0u8; 1024];
                let _ = stream.read(&mut request);
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(head.as_bytes()).unwrap();
                stream.write_all(body).unwrap();
            }
        });
        format!("http://{addr}/data.bin")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_hash_accepts_pinned_b3_digest() {
        let dep = Dep::Url {
            url: "https://example.com/data.bin".to_owned(),
            digest: Some(format!("b3:{}", "ab".repeat(32))),
        };
        let (key, digest) = dep.url_hash().unwrap().unwrap();
        assert_eq!(key, "https://example.com/data.bin");
        assert_eq!(digest, [0xab; 32]);
    }

    #[test]
    fn url_hash_rejects_non_b3_digest() {
        let sha = Dep::Url {
            url: "https://example.com/data.bin".to_owned(),
            digest: Some(format!("sha256:{}", "ab".repeat(32))),
        };
        assert!(matches!(
            sha.url_hash().unwrap_err(),
            CrabError::Configuration { .. }
        ));
    }

    #[test]
    fn url_hash_fetches_unpinned_http_body() {
        let url = test_support::serve_http_body_once(b"live-url-body");
        let dep = Dep::Url {
            url: url.clone(),
            digest: None,
        };

        let (key, digest) = dep.url_hash().unwrap().unwrap();
        assert_eq!(key, url);
        assert_eq!(digest, *blake3::hash(b"live-url-body").as_bytes());
    }

    #[test]
    fn url_hash_expands_remote_alias_to_backing_url() {
        let base_url = test_support::serve_http_body_once(b"remote-alias-body");
        let base_url = base_url.trim_end_matches("data.bin").to_owned();
        let dep = Dep::Url {
            url: "remote://datasets/raw.csv".to_owned(),
            digest: None,
        };
        let aliases = BTreeMap::from([("datasets".to_owned(), base_url)]);

        let (key, digest) = dep.url_hash_with_remote_aliases(&aliases).unwrap().unwrap();

        assert_eq!(key, "remote://datasets/raw.csv");
        assert_eq!(digest, *blake3::hash(b"remote-alias-body").as_bytes());
    }

    #[test]
    fn url_hash_reports_missing_remote_alias_config() {
        let dep = Dep::Url {
            url: "remote://datasets/raw.csv".to_owned(),
            digest: None,
        };

        match dep
            .url_hash_with_remote_aliases(&BTreeMap::new())
            .unwrap_err()
        {
            CrabError::Configuration { key, origin } => {
                assert_eq!(key, "workflow.remotes.datasets");
                assert!(origin.contains("remote://datasets/raw.csv"));
            }
            other => panic!("wrong error: {other:?}"),
        }
    }

    #[test]
    fn url_hash_expands_remote_alias_to_absolute_local_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("raw.csv"), b"local-alias-body").unwrap();
        let dep = Dep::Url {
            url: "remote://datasets/raw.csv".to_owned(),
            digest: None,
        };
        let aliases = BTreeMap::from([(
            "datasets".to_owned(),
            tmp.path().to_string_lossy().into_owned(),
        )]);

        let (key, digest) = dep.url_hash_with_remote_aliases(&aliases).unwrap().unwrap();

        assert_eq!(key, "remote://datasets/raw.csv");
        assert_eq!(digest, *blake3::hash(b"local-alias-body").as_bytes());
    }

    #[test]
    fn url_hash_hashes_file_url_body() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("data.bin");
        std::fs::write(&path, b"file-url-body").unwrap();
        let url = url::Url::from_file_path(&path).unwrap().to_string();
        let dep = Dep::Url {
            url: url.clone(),
            digest: None,
        };

        let (key, digest) = dep.url_hash().unwrap().unwrap();
        assert_eq!(key, url);
        assert_eq!(digest, *blake3::hash(b"file-url-body").as_bytes());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn object_store_url_hash_hashes_object_body() {
        use object_store::ObjectStore;

        let store = object_store::memory::InMemory::new();
        let location = ObjectPath::from("data.bin");
        store
            .put(&location, object_store::PutPayload::from("object-body"))
            .await
            .unwrap();

        let digest = hash_object_store_location(&store, &location)
            .await
            .expect("object store object should hash");

        assert_eq!(digest, *blake3::hash(b"object-body").as_bytes());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn object_store_url_hash_hashes_prefix_manifest() {
        use object_store::ObjectStore;

        let store = object_store::memory::InMemory::new();
        store
            .put(
                &ObjectPath::from("dataset/a.txt"),
                object_store::PutPayload::from("a"),
            )
            .await
            .unwrap();
        store
            .put(
                &ObjectPath::from("dataset/nested/b.txt"),
                object_store::PutPayload::from("b"),
            )
            .await
            .unwrap();
        store
            .put(
                &ObjectPath::from("dataset-neighbor.txt"),
                object_store::PutPayload::from("nope"),
            )
            .await
            .unwrap();
        let expected = crate::hasher::hash_tree_entries(&[
            crate::hasher::TreeEntry {
                path: PathBuf::from("a.txt"),
                kind: crate::hasher::TreeEntryKind::File,
                file_hash: *blake3::hash(b"a").as_bytes(),
                size: 1,
                mode: 0o644,
            },
            crate::hasher::TreeEntry {
                path: PathBuf::from("nested/b.txt"),
                kind: crate::hasher::TreeEntryKind::File,
                file_hash: *blake3::hash(b"b").as_bytes(),
                size: 1,
                mode: 0o644,
            },
        ]);

        let digest = hash_object_store_location(&store, &ObjectPath::from("dataset"))
            .await
            .expect("object store prefix should hash");

        assert_eq!(digest, expected);
    }

    #[test]
    fn url_hash_rejects_unpinned_non_http_scheme() {
        let dep = Dep::Url {
            url: "ssh://example.com/data.bin".to_owned(),
            digest: None,
        };
        assert!(matches!(
            dep.url_hash().unwrap_err(),
            CrabError::StageRemoteExecutionUnsupported
        ));
    }
}
