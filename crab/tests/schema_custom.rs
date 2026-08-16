use schemars::schema::RootSchema;
use serde_json::{Value, json};

pub fn add_live_smoke_evidence_constraints(mut schema: RootSchema) -> RootSchema {
    schema
        .schema
        .extensions
        .insert("allOf".to_owned(), live_smoke_evidence_all_of());
    schema
}

fn live_smoke_evidence_all_of() -> Value {
    json!([
        label_result_constraint(
            "writes-fenced",
            "replica.failover",
            json!({
                "type": "object",
                "required": ["active_active"],
                "properties": {
                    "active_active": {
                        "type": "object",
                        "required": ["writes_enabled"],
                        "properties": {
                            "writes_enabled": { "const": false }
                        }
                    }
                }
            }),
        ),
        label_result_constraint(
            "active-active-certification",
            "replica.certification",
            json!({
                "type": "object",
                "required": ["certified", "deep", "profile", "gates"],
                "properties": {
                    "certified": { "const": true },
                    "deep": { "const": true },
                    "profile": { "const": "active-active" },
                    "gates": {
                        "type": "array",
                        "minItems": 1,
                        "contains": {
                            "type": "object",
                            "required": ["code", "state"],
                            "properties": {
                                "code": { "const": "certification.active-active" },
                                "state": { "const": "passed" }
                            }
                        }
                    }
                }
            }),
        ),
        label_result_constraint(
            "provider-hydrate-copy",
            "replica.live-hydrate",
            json!({
                "type": "object",
                "required": ["provider", "copied_objects"],
                "properties": {
                    "provider": { "type": "string", "minLength": 1 },
                    "copied_objects": { "type": "integer", "minimum": 1 }
                }
            }),
        ),
        label_result_constraint(
            "provider-hydrate-primary-xorbs-deleted",
            "replica.live-hydrate",
            json!({
                "type": "object",
                "required": ["provider", "deleted_xorbs"],
                "properties": {
                    "provider": { "type": "string", "minLength": 1 },
                    "deleted_xorbs": { "type": "integer", "minimum": 1 }
                }
            }),
        ),
        label_result_constraint(
            "provider-hydrate-selected-replica",
            "hydrate",
            json!({
                "type": "object",
                "required": ["hydrated", "failed"],
                "properties": {
                    "hydrated": { "type": "integer", "minimum": 1 },
                    "failed": { "const": 0 }
                }
            }),
        ),
        label_result_constraint(
            "repair-worker-deployment",
            "replica.repair.worker-deployment",
            json!({
                "type": "object",
                "required": [
                    "artifact_ref",
                    "deployment_verified",
                    "service_template",
                    "template_blake3",
                    "command_blake3",
                    "command"
                ],
                "properties": {
                    "artifact_ref": { "type": "string", "minLength": 1 },
                    "deployment_verified": { "const": true },
                    "service_template": { "enum": ["systemd", "kubernetes"] },
                    "template_blake3": {
                        "type": "string",
                        "pattern": "^[0-9a-f]{64}$"
                    },
                    "command_blake3": {
                        "type": "string",
                        "pattern": "^[0-9a-f]{64}$"
                    },
                    "command": {
                        "type": "array",
                        "minItems": 6,
                        "items": { "type": "string" }
                    }
                }
            }),
        ),
        label_result_constraint(
            "production-load",
            "replica.production-load",
            json!({
                "type": "object",
                "required": [
                    "profile",
                    "xorb_count_source",
                    "xorb_count_before",
                    "xorb_count_after",
                    "xorb_count"
                ],
                "properties": {
                    "profile": { "const": "production" },
                    "xorb_count_source": { "const": "writer-store-delta" },
                    "xorb_count_before": { "type": "integer", "minimum": 0 },
                    "xorb_count_after": { "type": "integer", "minimum": 1 },
                    "xorb_count": { "type": "integer", "minimum": 1 }
                }
            }),
        ),
    ])
}

fn label_result_constraint(label: &str, schema: &str, data: Value) -> Value {
    json!({
        "if": {
            "properties": {
                "label": { "const": label }
            }
        },
        "then": {
            "properties": {
                "result": {
                    "type": "object",
                    "required": ["schema", "data"],
                    "properties": {
                        "schema": { "const": schema },
                        "data": data
                    }
                }
            }
        }
    })
}
