use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::stream::{self, BoxStream};
use futures_util::{StreamExt as _, TryStreamExt as _};
use object_store::path::Path;
use object_store::{
    Attributes, CopyOptions, GetOptions, GetResult, GetResultPayload, ListResult, MultipartUpload,
    ObjectMeta, ObjectStore, PutMultipartOptions, PutOptions, PutPayload, PutResult,
};
use reqwest::header::{AUTHORIZATION, CONTENT_LENGTH, RANGE};
use reqwest::{StatusCode, Url};

const STORE_NAME: &str = "crab-managed-gateway";
const CONTENT_HASH_HEADER: &str = "x-crab-content-blake3";

#[derive(Clone)]
pub(crate) struct GatewayObjectStore {
    client: reqwest::Client,
    service_url: Url,
    token: Arc<str>,
    repository_prefix: Arc<str>,
    staging_prefix: Option<Arc<str>>,
}

impl GatewayObjectStore {
    pub(crate) fn new(
        client: reqwest::Client,
        service_url: &str,
        token: &str,
        repository_prefix: &str,
        staging_prefix: Option<&str>,
    ) -> crate::Result<Self> {
        let service_url =
            Url::parse(&format!("{}/", service_url.trim_end_matches('/'))).map_err(|_| {
                crate::AuthStoreError::InvalidCredentials {
                    reason: "managed gateway URL is invalid".to_owned(),
                }
            })?;
        Ok(Self {
            client,
            service_url,
            token: Arc::from(token),
            repository_prefix: Arc::from(repository_prefix),
            staging_prefix: staging_prefix.map(Arc::from),
        })
    }

    fn relative_key(&self, location: &Path) -> object_store::Result<String> {
        let path = location.as_ref();
        let prefix = self.repository_prefix.as_ref();
        let relative = path
            .strip_prefix(prefix)
            .and_then(|value| value.strip_prefix('/'))
            .filter(|value| !value.is_empty())
            .ok_or_else(|| object_store::Error::PermissionDenied {
                path: path.to_owned(),
                source: "path is outside the managed repository grant".into(),
            })?;
        Ok(relative.to_owned())
    }

    fn object_url(&self, location: &Path) -> object_store::Result<Url> {
        let relative = self.route_key(location)?;
        let mut url = self.service_url.join("objects/").map_err(generic_error)?;
        url.path_segments_mut()
            .map_err(|()| generic_error("managed gateway URL cannot contain path segments"))?
            .pop_if_empty()
            .extend(relative.split('/'));
        Ok(url)
    }

    fn route_key(&self, location: &Path) -> object_store::Result<String> {
        if let Some(staging_prefix) = &self.staging_prefix {
            let staged_objects = format!("{staging_prefix}/objects/{}/", self.repository_prefix);
            if let Some(relative) = location.as_ref().strip_prefix(&staged_objects)
                && !relative.is_empty()
            {
                return Ok(relative.to_owned());
            }
        }
        self.relative_key(location)
    }

    fn request(&self, method: reqwest::Method, url: Url) -> reqwest::RequestBuilder {
        self.client
            .request(method, url)
            .header(AUTHORIZATION, format!("Bearer {}", self.token))
    }

    async fn metadata(&self, location: &Path) -> object_store::Result<ObjectMeta> {
        let response = self
            .request(reqwest::Method::HEAD, self.object_url(location)?)
            .send()
            .await
            .map_err(generic_error)?;
        map_status(response.status(), location)?;
        let size = response
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(|| generic_error("managed gateway HEAD omitted Content-Length"))?;
        Ok(ObjectMeta {
            location: location.clone(),
            last_modified: unix_epoch()?,
            size,
            e_tag: None,
            version: None,
        })
    }
}

impl fmt::Debug for GatewayObjectStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GatewayObjectStore")
            .field("service_url", &self.service_url)
            .field("repository_prefix", &self.repository_prefix)
            .field("staging", &self.staging_prefix.is_some())
            .finish_non_exhaustive()
    }
}

impl fmt::Display for GatewayObjectStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "GatewayObjectStore({})", self.service_url)
    }
}

#[async_trait]
impl ObjectStore for GatewayObjectStore {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        if self.staging_prefix.is_none() || !matches!(opts.mode, object_store::PutMode::Create) {
            return Err(read_only(location));
        }
        let content_length = payload.content_length();
        let mut hasher = blake3::Hasher::new();
        for chunk in &payload {
            hasher.update(chunk);
        }
        let body = reqwest::Body::wrap_stream(stream::iter(
            payload.into_iter().map(Ok::<_, std::io::Error>),
        ));
        let response = self
            .request(reqwest::Method::PUT, self.object_url(location)?)
            .header(CONTENT_LENGTH, content_length)
            .header(CONTENT_HASH_HEADER, hasher.finalize().to_hex().as_str())
            .body(body)
            .send()
            .await
            .map_err(generic_error)?;
        map_status(response.status(), location)?;
        Ok(PutResult {
            e_tag: None,
            version: None,
            extensions: Default::default(),
        })
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        _opts: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        Err(read_only(location))
    }

    async fn get_opts(
        &self,
        location: &Path,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        let meta = self.metadata(location).await?;
        options.check_preconditions(&meta)?;
        let range = match options.range {
            Some(range) => range.as_range(meta.size).map_err(generic_error)?,
            None => 0..meta.size,
        };
        if options.head {
            return Ok(GetResult {
                payload: GetResultPayload::Stream(stream::empty().boxed()),
                meta,
                range: 0..0,
                attributes: Attributes::default(),
                extensions: Default::default(),
            });
        }
        let mut request = self.request(reqwest::Method::GET, self.object_url(location)?);
        if range.start != 0 || range.end != meta.size {
            request = request.header(RANGE, format!("bytes={}-{}", range.start, range.end - 1));
        }
        let response = request.send().await.map_err(generic_error)?;
        map_status(response.status(), location)?;
        let payload = response.bytes_stream().map_err(generic_error).boxed();
        Ok(GetResult {
            payload: GetResultPayload::Stream(payload),
            meta,
            range,
            attributes: Attributes::default(),
            extensions: Default::default(),
        })
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        locations
            .map(|location| location.and_then(|path| Err(read_only(&path))))
            .boxed()
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        let this = self.clone();
        let prefix = prefix.cloned();
        stream::once(async move { this.list_objects(prefix.as_ref()).await })
            .map(|result| match result {
                Ok(objects) => stream::iter(objects.into_iter().map(Ok)).boxed(),
                Err(error) => stream::once(async move { Err(error) }).boxed(),
            })
            .flatten()
            .boxed()
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> object_store::Result<ListResult> {
        Ok(ListResult {
            common_prefixes: Vec::new(),
            objects: self.list_objects(prefix).await?,
            extensions: Default::default(),
        })
    }

    async fn copy_opts(
        &self,
        _from: &Path,
        to: &Path,
        _options: CopyOptions,
    ) -> object_store::Result<()> {
        Err(read_only(to))
    }
}

impl GatewayObjectStore {
    async fn list_objects(&self, prefix: Option<&Path>) -> object_store::Result<Vec<ObjectMeta>> {
        let relative = match prefix {
            Some(path) if path.as_ref() == self.repository_prefix.as_ref() => String::new(),
            Some(path) => self.relative_key(path)?,
            None => String::new(),
        };
        let mut url = self.service_url.join("list").map_err(generic_error)?;
        if !relative.is_empty() {
            url.query_pairs_mut().append_pair("prefix", &relative);
        }
        let response = self
            .request(reqwest::Method::GET, url)
            .send()
            .await
            .map_err(generic_error)?;
        let status = response.status();
        if !status.is_success() {
            let empty = Path::from("");
            return map_status(status, prefix.unwrap_or(&empty)).and(Ok(Vec::new()));
        }
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.try_next().await.map_err(generic_error)? {
            if bytes.len().saturating_add(chunk.len()) > 4 * 1024 * 1024 {
                return Err(generic_error(
                    "managed gateway list response exceeds the client limit",
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        let body = crab_auth::GatewayObjectPage::from_slice(&bytes).map_err(generic_error)?;
        body.objects
            .into_iter()
            .map(|object| {
                if !relative.is_empty()
                    && object.key != relative
                    && !object
                        .key
                        .strip_prefix(&relative)
                        .is_some_and(|suffix| suffix.starts_with('/'))
                {
                    return Err(generic_error(
                        "managed gateway listed an object outside the requested prefix",
                    ));
                }
                Ok(ObjectMeta {
                    location: Path::from(format!("{}/{}", self.repository_prefix, object.key)),
                    last_modified: unix_epoch()?,
                    size: object.size,
                    e_tag: None,
                    version: None,
                })
            })
            .collect()
    }
}

fn map_status(status: StatusCode, location: &Path) -> object_store::Result<()> {
    let path = location.to_string();
    match status {
        status if status.is_success() => Ok(()),
        StatusCode::UNAUTHORIZED => Err(object_store::Error::Unauthenticated {
            path,
            source: "managed gateway rejected its transfer token".into(),
        }),
        StatusCode::FORBIDDEN => Err(object_store::Error::PermissionDenied {
            path,
            source: "managed gateway denied this repository object".into(),
        }),
        StatusCode::NOT_FOUND => Err(object_store::Error::NotFound {
            path,
            source: "managed gateway object was not found".into(),
        }),
        StatusCode::CONFLICT => Err(object_store::Error::AlreadyExists {
            path,
            source: "managed gateway immutable object already exists".into(),
        }),
        _ => Err(generic_error(format!(
            "managed gateway returned HTTP {}",
            status.as_u16()
        ))),
    }
}

fn read_only(location: &Path) -> object_store::Error {
    object_store::Error::PermissionDenied {
        path: location.to_string(),
        source: "read transfer grant cannot mutate objects".into(),
    }
}

fn unix_epoch<T>() -> object_store::Result<T>
where
    T: std::str::FromStr,
    T::Err: fmt::Display,
{
    "1970-01-01T00:00:00Z".parse().map_err(generic_error)
}

fn generic_error(error: impl fmt::Display) -> object_store::Error {
    object_store::Error::Generic {
        store: STORE_NAME,
        source: std::io::Error::other(error.to_string()).into(),
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    use futures_util::TryStreamExt as _;
    use object_store::ObjectStoreExt as _;

    use super::*;

    #[tokio::test]
    async fn gateway_maps_granted_physical_paths_to_relative_object_routes() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let expected = [
                ("HEAD /gateway/v1/objects/manifest ", "", Some(2)),
                ("GET /gateway/v1/objects/manifest ", "ok", None),
                (
                    "GET /gateway/v1/list?prefix=packs ",
                    r#"{"schema_version":1,"objects":[{"key":"packs/a.pack","size":7}]}"#,
                    None,
                ),
            ];
            for (request_line, body, content_length) in expected {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = vec![0; 4096];
                let read = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..read]);
                assert!(request.starts_with(request_line), "{request}");
                assert!(request.contains("authorization: Bearer gateway-token\r\n"));
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
                    content_length.unwrap_or(body.len())
                )
                .unwrap();
            }
        });
        let store = GatewayObjectStore::new(
            reqwest::Client::new(),
            &format!("http://{address}/gateway/v1"),
            "gateway-token",
            "environments/prod/repositories/repo-1",
            None,
        )
        .unwrap();
        let object = Path::from("environments/prod/repositories/repo-1/manifest");

        let bytes = store.get(&object).await.unwrap().bytes().await.unwrap();
        assert_eq!(bytes.as_ref(), b"ok");
        let prefix = Path::from("environments/prod/repositories/repo-1/packs");
        let objects = store
            .list(Some(&prefix))
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        assert_eq!(
            objects[0].location,
            Path::from("environments/prod/repositories/repo-1/packs/a.pack")
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn gateway_push_routes_staged_physical_writes_to_immutable_object_api() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = vec![0; 4096];
            let read = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request.starts_with("PUT /gateway/v1/objects/xorbs/a "));
            assert!(request.contains("authorization: Bearer push-token\r\n"));
            assert!(request.contains(&format!(
                "x-crab-content-blake3: {}\r\n",
                blake3::hash(b"xorb").to_hex()
            )));
            write!(
                stream,
                "HTTP/1.1 201 Created\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
        });
        let repository_prefix = "environments/prod/repositories/repo-1";
        let staging_prefix = format!("{repository_prefix}/staging/push-1");
        let store = GatewayObjectStore::new(
            reqwest::Client::new(),
            &format!("http://{address}/gateway/v1"),
            "push-token",
            repository_prefix,
            Some(&staging_prefix),
        )
        .unwrap();
        let staged = Path::from(format!(
            "{staging_prefix}/objects/{repository_prefix}/xorbs/a"
        ));

        store
            .put_opts(
                &staged,
                PutPayload::from_static(b"xorb"),
                PutOptions::from(object_store::PutMode::Create),
            )
            .await
            .unwrap();

        server.join().unwrap();
    }

    #[test]
    fn gateway_rejects_paths_outside_the_granted_repository_without_network_io() {
        let store = GatewayObjectStore::new(
            reqwest::Client::new(),
            "http://127.0.0.1:1/gateway/v1",
            "gateway-token",
            "repositories/repo-1",
            None,
        )
        .unwrap();

        let error = store
            .object_url(&Path::from("repositories/repo-10/manifest"))
            .unwrap_err();

        assert!(matches!(
            error,
            object_store::Error::PermissionDenied { .. }
        ));
    }
}
