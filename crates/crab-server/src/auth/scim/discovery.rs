use std::sync::Arc;

use axum::{Router, extract::Path, http::StatusCode, response::Response, routing::get};
use serde_json::{Value, json};

use super::{Error, ROOT, Result, USER, list_response, response};
use crate::server::Server;

const CONFIG: &str = "urn:ietf:params:scim:schemas:core:2.0:ServiceProviderConfig";
const RESOURCE_TYPE: &str = "urn:ietf:params:scim:schemas:core:2.0:ResourceType";
const SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:Schema";

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new()
        .route("/ServiceProviderConfig", get(config))
        .route(
            "/ResourceTypes",
            get(|| async { list_response(vec![user_type()], 1, 1) }),
        )
        .route(
            "/ResourceTypes/User",
            get(|| async { response(StatusCode::OK, user_type()) }),
        )
        .route(
            "/Schemas",
            get(|| async { list_response(vec![user_schema()], 1, 1) }),
        )
        .route("/Schemas/{id}", get(schema))
}

async fn config() -> Response {
    response(
        StatusCode::OK,
        json!({
            "schemas": [CONFIG],
            "patch": {"supported": true},
            "bulk": {"supported": false, "maxOperations": 0, "maxPayloadSize": 0},
            "filter": {"supported": true, "maxResults": 100},
            "changePassword": {"supported": false},
            "sort": {"supported": false},
            "etag": {"supported": true},
            "authenticationSchemes": [{
                "type": "oauthbearertoken",
                "name": "Bearer token",
                "description": "The operator-configured SCIM bearer token",
                "specUri": "https://www.rfc-editor.org/rfc/rfc6750",
                "primary": true
            }],
            "meta": {"resourceType": "ServiceProviderConfig", "location": format!("{ROOT}/ServiceProviderConfig")},
        }),
    )
}

fn user_type() -> Value {
    json!({
        "schemas": [RESOURCE_TYPE],
        "id": "User",
        "name": "User",
        "description": "An account bound to the configured OIDC issuer",
        "endpoint": "/Users",
        "schema": USER,
        "schemaExtensions": [],
        "meta": {"resourceType": "ResourceType", "location": format!("{ROOT}/ResourceTypes/User")},
    })
}

async fn schema(Path(id): Path<String>) -> Result<Response> {
    if id != USER {
        return Err(Error::not_found());
    }
    Ok(response(StatusCode::OK, user_schema()))
}

fn attribute(name: &str, kind: &str) -> Value {
    json!({
        "name": name,
        "type": kind,
        "multiValued": false,
        "required": false,
        "caseExact": false,
        "mutability": "readWrite",
        "returned": "default",
        "uniqueness": "none",
    })
}

fn user_schema() -> Value {
    let mut username = attribute("userName", "string");
    username["required"] = json!(true);
    username["uniqueness"] = json!("server");
    let mut name = attribute("name", "complex");
    name["subAttributes"] = json!(
        [
            "formatted",
            "familyName",
            "givenName",
            "middleName",
            "honorificPrefix",
            "honorificSuffix"
        ]
        .map(|field| attribute(field, "string"))
    );
    let mut emails = attribute("emails", "complex");
    emails["multiValued"] = json!(true);
    emails["subAttributes"] = json!([
        attribute("value", "string"),
        attribute("type", "string"),
        attribute("primary", "boolean"),
    ]);
    json!({
        "schemas": [SCHEMA],
        "id": USER,
        "name": "User",
        "attributes": [username, attribute("displayName", "string"), attribute("active", "boolean"), name, emails],
        "meta": {"resourceType": "Schema", "location": format!("{ROOT}/Schemas/{USER}")},
    })
}
