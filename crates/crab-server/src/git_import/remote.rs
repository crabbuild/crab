//! Protocol-v2 smart HTTP client for one validated, allowlisted Git source.

use std::time::Duration;

use bytes::Bytes;
use crab_git::fetch_wire::{self, AdvertisedRef, Capabilities};
use futures_util::{Stream, StreamExt, TryStreamExt};
use gix_hash::ObjectId;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderValue};

use super::ImportError;

const AGENT: &str = "git/crab-server";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
// Bounds a stalled source without limiting how long a large pack may take.
const READ_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_ADVERTISEMENT_BYTES: usize = 64 * 1024;
const MAX_REF_LISTING_BYTES: usize = 64 * 1024 * 1024;

/// A Git source reached over smart HTTP with protocol version 2.
pub(super) struct Remote {
    client: reqwest::Client,
    url: String,
    authorization: Option<HeaderValue>,
}

impl Remote {
    /// Prepare requests for a normalized `http(s)://…/repository.git` source.
    pub(super) fn new(source: &str, token: Option<&str>) -> Result<Self, ImportError> {
        if !source.starts_with("https://") && !source.starts_with("http://") {
            return Err(ImportError::Invalid(
                "Git imports fetch over HTTPS; use the repository's HTTPS clone URL",
            ));
        }
        let client = reqwest::Client::builder()
            .user_agent(AGENT)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .build()
            .map_err(|error| ImportError::Git(format!("Git client setup failed: {error}")))?;
        let authorization = token
            .map(|token| {
                let mut value = HeaderValue::from_str(&format!("Bearer {token}"))
                    .map_err(|_| ImportError::Invalid("the Git source token is invalid"))?;
                value.set_sensitive(true);
                Ok::<_, ImportError>(value)
            })
            .transpose()?;
        Ok(Self {
            client,
            url: source.trim_end_matches('/').to_owned(),
            authorization,
        })
    }

    /// Read the server's protocol-v2 capability advertisement.
    pub(super) async fn capabilities(&self) -> Result<Capabilities, ImportError> {
        let request = self
            .authorized(
                self.client
                    .get(format!("{}/info/refs?service=git-upload-pack", self.url)),
            )
            .header("Git-Protocol", "version=2");
        let body = bounded(send(request).await?, MAX_ADVERTISEMENT_BYTES).await?;
        fetch_wire::parse_advertisement(&body).map_err(wire)
    }

    /// List refs under `prefixes`; the client still filters the result.
    pub(super) async fn list_refs(
        &self,
        prefixes: &[&str],
    ) -> Result<Vec<AdvertisedRef>, ImportError> {
        let response = self
            .command(fetch_wire::encode_ls_refs(AGENT, prefixes))
            .await?;
        let body = bounded(response, MAX_REF_LISTING_BYTES).await?;
        fetch_wire::parse_ls_refs(&body).map_err(wire)
    }

    /// Request a complete pack for `wants`, excluding objects reachable from `haves`.
    pub(super) async fn fetch(
        &self,
        wants: &[ObjectId],
        haves: &[ObjectId],
    ) -> Result<impl Stream<Item = Result<Bytes, ImportError>> + Send + 'static, ImportError> {
        let response = self
            .command(fetch_wire::encode_fetch(AGENT, wants, haves))
            .await?;
        Ok(response
            .bytes_stream()
            .map_err(|error| ImportError::Git(format!("Git source stream failed: {error}"))))
    }

    async fn command(&self, body: Vec<u8>) -> Result<reqwest::Response, ImportError> {
        let request = self
            .authorized(self.client.post(format!("{}/git-upload-pack", self.url)))
            .header("Git-Protocol", "version=2")
            .header(CONTENT_TYPE, "application/x-git-upload-pack-request")
            .header(ACCEPT, "application/x-git-upload-pack-result")
            .body(body);
        send(request).await
    }

    fn authorized(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.authorization {
            Some(value) => request.header(AUTHORIZATION, value.clone()),
            None => request,
        }
    }
}

async fn send(request: reqwest::RequestBuilder) -> Result<reqwest::Response, ImportError> {
    let response = request
        .send()
        .await
        .map_err(|error| ImportError::Git(format!("Git source request failed: {error}")))?;
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    Err(ImportError::Git(match status.as_u16() {
        401 | 403 => "The Git source denied access; check the token".into(),
        404 => "The Git source repository was not found".into(),
        code if status.is_redirection() => {
            format!("The Git source redirected the request ({code}); use its final HTTPS URL")
        }
        code => format!("The Git source returned HTTP {code}"),
    }))
}

async fn bounded(response: reqwest::Response, maximum: usize) -> Result<Vec<u8>, ImportError> {
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|error| ImportError::Git(format!("Git source read failed: {error}")))?;
        if body.len() + chunk.len() > maximum {
            return Err(ImportError::Git(
                "The Git source sent an oversized response".into(),
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn wire(error: fetch_wire::FetchWireError) -> ImportError {
    ImportError::Git(error.to_string())
}
