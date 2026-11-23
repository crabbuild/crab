use std::{sync::Arc, time::Duration};

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{actor, require_owner, target};
use crate::{
    api::read_admission,
    app::{self, Error, Result},
    auth::Principal,
    server::Server,
};

mod event;
mod scan;

#[cfg(test)]
pub(in crate::api) use event::AuditRepository;
pub(in crate::api) use event::{AuditAction, AuditEvent};

use scan::Cursor;

const EXPORT_LIMIT: usize = 500;
const EXPORT_BUDGET: usize = 512;

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new().route("/api/organizations/{id}/audit-log", get(list))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Parameters {
    action: Option<AuditAction>,
    actor: Option<Uuid>,
    account: Option<Uuid>,
    repository: Option<String>,
    since_ms: Option<u64>,
    until_ms: Option<u64>,
    after: Option<String>,
    limit: Option<u8>,
    export: Option<Export>,
}

impl Parameters {
    fn cursor_context(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(&(
            self.action,
            self.actor,
            self.account,
            &self.repository,
            self.since_ms,
            self.until_ms,
        ))?)
    }

    fn matches(&self, event: &AuditEvent) -> bool {
        self.action.is_none_or(|action| event.action == action)
            && self.actor.is_none_or(|actor| event.actor == Some(actor))
            && self
                .account
                .is_none_or(|account| event.account == Some(account))
            && self.repository.as_ref().is_none_or(|name| {
                event
                    .repository
                    .as_ref()
                    .is_some_and(|repo| name.eq_ignore_ascii_case(&repo.name))
            })
            && self
                .since_ms
                .is_none_or(|since| event.occurred_at_ms >= since)
            && self
                .until_ms
                .is_none_or(|until| event.occurred_at_ms < until)
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Export {
    Json,
    Csv,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(in crate::api) struct AuditPage {
    items: Vec<AuditEvent>,
    next: Option<String>,
}

async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(organization): Path<Uuid>,
    Query(params): Query<Parameters>,
) -> Result<Response> {
    let limit = usize::from(params.limit.unwrap_or(30));
    if !(1..=50).contains(&limit)
        || params.export.is_some() && params.limit.is_some()
        || params
            .repository
            .as_ref()
            .is_some_and(|name| !crate::config::valid_repository_component(name))
        || params
            .since_ms
            .zip(params.until_ms)
            .is_some_and(|(since, until)| since >= until)
    {
        return Err(Error::Invalid(
            "Choose valid audit filters and a page size from 1 to 50",
        ));
    }
    let address = target(organization)?;
    let Ok(admission) = read_admission::acquire_bulk(&server) else {
        return Ok(read_admission::busy());
    };
    let owner = actor(&server, &principal).await?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    require_owner(&cells, address, owner).await?;
    let catalog = server.catalog().ok_or(Error::NotFound)?;

    let mut key = blake3::Hasher::new_keyed(&server.cursor_key);
    key.update(b"crab-server.organization.audit-log.v1");
    key.update(organization.as_bytes());
    key.update(owner.as_bytes());
    key.update(&[u8::from(params.export.is_some())]);
    key.update(&params.cursor_context()?);
    let cursor = params
        .after
        .as_deref()
        .map(|after| decode(&key, after))
        .transpose()?;
    let (limit, budget) = if params.export.is_some() {
        (EXPORT_LIMIT, EXPORT_BUDGET)
    } else {
        (limit, 80)
    };
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        scan::search(
            &cells,
            catalog,
            organization,
            owner,
            cursor,
            &params,
            limit,
            budget,
        ),
    )
    .await;
    let Ok(result) = result else {
        return Ok(failure(
            StatusCode::GATEWAY_TIMEOUT,
            "Audit history took too long to read. Wait briefly and try again.",
        ));
    };
    let result = result?;
    // Catalog and Directory reads can outlast an owner grant.
    require_owner(&cells, address, owner).await?;
    app::account(&principal)?;
    let next = result.next.map(|cursor| encode(&key, cursor)).transpose()?;
    let response = if let Some(format) = params.export {
        export(format, result.items, next)?
    } else {
        Json(AuditPage {
            items: result.items,
            next,
        })
        .into_response()
    };
    Ok(read_admission::retain(response, admission))
}

fn encode(key: &blake3::Hasher, cursor: Cursor) -> Result<String> {
    let data = serde_json::to_vec(&cursor)?;
    let mut mac = key.clone();
    mac.update(&data);
    Ok(format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(data),
        mac.finalize().to_hex()
    ))
}

fn decode(key: &blake3::Hasher, value: &str) -> Result<Cursor> {
    let invalid = || Error::Invalid("Invalid audit log cursor");
    let (data, tag) = value
        .split_once('.')
        .filter(|_| value.len() <= 512)
        .ok_or_else(invalid)?;
    let data = URL_SAFE_NO_PAD.decode(data).map_err(|_| invalid())?;
    let tag = blake3::Hash::from_hex(tag).map_err(|_| invalid())?;
    let mut mac = key.clone();
    mac.update(&data);
    if mac.finalize() != tag {
        return Err(Error::PageChanged);
    }
    serde_json::from_slice(&data).map_err(|_| invalid())
}

fn failure(status: StatusCode, message: &'static str) -> Response {
    (
        status,
        Json(serde_json::json!({"error": {"code":"audit_log_unavailable", "message":message}})),
    )
        .into_response()
}

fn export(format: Export, items: Vec<AuditEvent>, next: Option<String>) -> Result<Response> {
    let count = items.len();
    let (content_type, disposition, bytes) = match format {
        Export::Json => (
            "application/json",
            "attachment; filename=crab-audit-log.json",
            serde_json::to_vec(&items)?,
        ),
        Export::Csv => {
            let mut out = String::from(
                "event,occurred_at_ms,action,actor_id,operator,account_id,team_id,project_id,repository_id,repository,detail\r\n",
            );
            for item in items {
                let action = serde_json::to_value(item.action)?;
                let action = action
                    .as_str()
                    .ok_or(Error::CellContract("Invalid audit action"))?;
                let id = |id: Option<Uuid>| id.map(|id| id.to_string()).unwrap_or_default();
                let row = [
                    item.id,
                    item.occurred_at_ms.to_string(),
                    action.to_owned(),
                    id(item.actor),
                    item.operator.to_string(),
                    id(item.account),
                    id(item.team),
                    id(item.project),
                    id(item.repository.as_ref().map(|repo| repo.id)),
                    item.repository.map(|repo| repo.name).unwrap_or_default(),
                    item.detail.unwrap_or_default(),
                ];
                for (index, value) in row.into_iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    out.push('"');
                    // Spreadsheet formula characters are unsafe even in quoted fields.
                    if value.trim_start().starts_with(['=', '+', '-', '@'])
                        || value.starts_with(['\t', '\r', '\n'])
                    {
                        out.push('\'');
                    }
                    out.push_str(&value.replace('"', "\"\""));
                    out.push('"');
                }
                out.push_str("\r\n");
            }
            (
                "text/csv; charset=utf-8",
                "attachment; filename=crab-audit-log.csv",
                out.into_bytes(),
            )
        }
    };
    let mut response = (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CONTENT_DISPOSITION, disposition),
            (header::CACHE_CONTROL, "private, no-store"),
        ],
        bytes,
    )
        .into_response();
    let headers = response.headers_mut();
    headers.insert("x-crab-audit-count", header::HeaderValue::from(count));
    if let Some(next) = next {
        let value = header::HeaderValue::from_str(&next)
            .map_err(|_| Error::CellContract("Invalid encoded audit cursor header"))?;
        headers.insert("x-crab-audit-next", value);
    }
    Ok(response)
}
