use std::time::Duration;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use cellar::sdk::IngressHandle;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    Address, Error, Result,
    application::{self, Mutation, MutationIdentity, Read},
};

#[derive(Clone)]
pub(crate) struct CellClient {
    ingress: IngressHandle,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct Receipt {
    pub target: Address,
    pub revision: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Committed<T> {
    pub output: T,
    pub receipt: Receipt,
}

pub(crate) struct Observed<T> {
    pub output: T,
    pub receipt: Receipt,
}

pub(crate) struct RepositoryCell {
    pub target: Uuid,
    pub client: CellClient,
}

impl CellClient {
    pub(crate) fn new(ingress: IngressHandle) -> Self {
        Self { ingress }
    }

    pub(crate) fn repository(&self, repository: Uuid) -> Result<RepositoryCell> {
        if self.ingress.is_closed() {
            return Err(Error::Unavailable);
        }
        Ok(RepositoryCell {
            target: repository,
            client: self.clone(),
        })
    }

    pub(crate) async fn verify_repositories(&self, repositories: Vec<Uuid>) -> crate::Result<()> {
        for repository in repositories {
            self.query::<super::repository::GetRepositoryLifecycle>(&repository, None, ())
                .await?;
        }
        Ok(())
    }

    pub(crate) fn close_admission(&self) {
        self.ingress.close_admission();
    }
    pub(crate) fn is_closed(&self) -> bool {
        self.ingress.is_closed()
    }

    pub(crate) async fn initialize(&self, target: impl Into<Address>) -> Result<()> {
        self.call(target.into(), application::INITIALIZE, None, (), None)
            .await?;
        Ok(())
    }

    pub(crate) async fn command<C: Mutation>(
        &self,
        target: impl Into<Address>,
        identity: MutationIdentity,
        input: C::Input,
    ) -> Result<Committed<C::Output>> {
        let target = target.into();
        if target.class() != C::CLASS {
            return Err(Error::Invalid("command belongs to another Cell authority"));
        }
        let reply = self
            .call(target, C::ID, Some(identity), input, None)
            .await?;
        let output = serde_json::from_value(reply.output)?;
        Ok(Committed {
            output,
            receipt: Receipt {
                target,
                revision: reply.revision,
            },
        })
    }

    pub(crate) async fn query<Q: Read>(
        &self,
        target: impl Into<Address>,
        minimum: Option<Receipt>,
        input: Q::Input,
    ) -> Result<Observed<Q::Output>> {
        let target = target.into();
        if target.class() != Q::CLASS {
            return Err(Error::Invalid("query belongs to another Cell authority"));
        }
        if minimum.is_some_and(|receipt| receipt.target != target) {
            return Err(Error::Invalid("read receipt belongs to another Cell"));
        }
        let reply = self
            .call(
                target,
                Q::ID,
                None,
                input,
                minimum.map(|receipt| receipt.revision),
            )
            .await?;
        let output = serde_json::from_value(reply.output)?;
        Ok(Observed {
            output,
            receipt: Receipt {
                target,
                revision: reply.revision,
            },
        })
    }

    async fn call(
        &self,
        target: Address,
        operation: u32,
        mutation: Option<MutationIdentity>,
        input: impl Serialize,
        minimum: Option<u64>,
    ) -> Result<application::Reply> {
        let request = application::Request {
            target,
            operation,
            mutation,
            input: serde_json::to_value(input)?,
            minimum,
        };
        let bytes = serde_json::to_vec(&request)?;
        if bytes.len() > 4 * 1024 * 1024 {
            return Err(Error::Invalid("Cell request exceeds its byte limit"));
        }
        let request = Request::post(format!("http://crab{}", target.path()))
            .header("content-type", "application/json")
            .body(Body::from(bytes))
            .map_err(Error::sdk)?;
        let result = tokio::time::timeout(Duration::from_secs(30), async {
            let response = self.ingress.call(request).await;
            let status = response.status();
            let bytes = axum::body::to_bytes(Body::new(response.into_body()), 2 * 1024 * 1024)
                .await
                .map_err(Error::sdk)?;
            match status {
                StatusCode::OK => Ok(serde_json::from_slice(&bytes)?),
                StatusCode::CONFLICT => Err(Error::RequestConflict),
                StatusCode::GONE => Err(Error::Expired),
                StatusCode::PRECONDITION_FAILED => Err(Error::StaleRead),
                StatusCode::PRECONDITION_REQUIRED => Err(Error::NotInitialized),
                StatusCode::FAILED_DEPENDENCY => Err(Error::IndexUnavailable),
                StatusCode::RANGE_NOT_SATISFIABLE => Err(Error::StaleSearch),
                StatusCode::BAD_REQUEST => Err(Error::Invalid("Cell rejected the request")),
                _ => Err(Error::Unavailable),
            }
        })
        .await;
        match result {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(
                error @ (Error::RequestConflict
                | Error::Expired
                | Error::Invalid(_)
                | Error::NotInitialized
                | Error::IndexUnavailable
                | Error::StaleSearch),
            )) => Err(error),
            Ok(Err(source)) if mutation.is_some() => Err(Error::Pending {
                source: Box::new(source),
            }),
            Err(source) if mutation.is_some() => Err(Error::Pending {
                source: Box::new(source),
            }),
            Ok(Err(error)) => Err(error),
            Err(error) => Err(Error::sdk(error)),
        }
    }
}
