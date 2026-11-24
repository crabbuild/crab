use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Error, Result, USER, User};

const PATCH: &str = "urn:ietf:params:scim:api:messages:2.0:PatchOp";

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Profile {
    pub(super) user_name: String,
    pub(super) active: bool,
    pub(super) display_name: Option<String>,
    pub(super) name: Option<Name>,
    pub(super) emails: Vec<Email>,
}

impl Profile {
    #[cfg(test)]
    pub(super) fn new(user_name: String) -> Self {
        Self {
            user_name,
            active: true,
            display_name: None,
            name: None,
            emails: Vec::new(),
        }
    }

    fn validate(&self) -> Result<()> {
        if !valid_text(&self.user_name, 256) || self.user_name.trim() != self.user_name {
            return Err(Error::invalid(
                "invalidValue",
                "A valid userName is required",
            ));
        }
        if self
            .display_name
            .as_deref()
            .is_some_and(|value| !valid_text(value, 256))
            || self.name.as_ref().is_some_and(|name| {
                [
                    &name.formatted,
                    &name.family_name,
                    &name.given_name,
                    &name.middle_name,
                    &name.honorific_prefix,
                    &name.honorific_suffix,
                ]
                .into_iter()
                .flatten()
                .any(|value| !valid_text(value, 256))
            })
            || self.emails.len() > 10
            || self
                .emails
                .iter()
                .filter(|email| email.primary == Some(true))
                .count()
                > 1
            || self.emails.iter().any(|email| {
                !valid_text(&email.value, 320)
                    || email
                        .kind
                        .as_deref()
                        .is_some_and(|value| !valid_text(value, 30))
            })
        {
            return Err(Error::invalid(
                "invalidValue",
                "Invalid SCIM profile attributes",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Name {
    #[serde(skip_serializing_if = "Option::is_none")]
    formatted: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    family_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    given_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    middle_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    honorific_prefix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    honorific_suffix: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Email {
    value: String,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    primary: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct UserInput {
    schemas: Vec<String>,
    pub(super) external_id: Option<String>,
    user_name: String,
    active: Option<bool>,
    display_name: Option<String>,
    name: Option<Name>,
    #[serde(default)]
    emails: Vec<Email>,
}

impl UserInput {
    pub(super) fn profile(&self, current_active: bool) -> Result<Profile> {
        if !self.schemas.iter().any(|schema| schema == USER) {
            return Err(Error::invalid(
                "invalidSyntax",
                "The core User schema is required",
            ));
        }
        let profile = Profile {
            user_name: self.user_name.clone(),
            active: self.active.unwrap_or(current_active),
            display_name: self.display_name.clone(),
            name: self.name.clone(),
            emails: self.emails.clone(),
        };
        profile.validate()?;
        Ok(profile)
    }
}

pub(super) fn valid_subject(value: &str) -> bool {
    valid_text(value, 1024)
}

fn valid_text(value: &str, max: usize) -> bool {
    !value.is_empty() && value.chars().count() <= max && !value.chars().any(char::is_control)
}

#[derive(Deserialize)]
pub(super) struct Patch {
    schemas: Vec<String>,
    #[serde(rename = "Operations")]
    operations: Vec<Operation>,
}

#[derive(Deserialize)]
struct Operation {
    op: String,
    path: Option<String>,
    #[serde(default)]
    value: Value,
}

impl Patch {
    pub(super) fn apply(&self, user: &User) -> Result<Profile> {
        if !self.schemas.iter().any(|schema| schema == PATCH)
            || !(1..=32).contains(&self.operations.len())
        {
            return Err(Error::invalid(
                "invalidSyntax",
                "A PatchOp with 1 to 32 operations is required",
            ));
        }
        let mut profile = user.profile.clone();
        for operation in &self.operations {
            match operation.op.to_ascii_lowercase().as_str() {
                "add" | "replace" => match operation.path.as_deref() {
                    Some(path) if !operation.value.is_null() => {
                        update(
                            &mut profile,
                            user,
                            path,
                            operation.value.clone(),
                            operation.op.eq_ignore_ascii_case("add"),
                        )?;
                    }
                    Some(_) => {
                        return Err(Error::invalid("invalidValue", "A patch value is required"));
                    }
                    None => {
                        let changes = operation.value.as_object().ok_or_else(|| {
                            Error::invalid(
                                "invalidValue",
                                "An object is required for a patch without a path",
                            )
                        })?;
                        for (path, value) in changes {
                            update(
                                &mut profile,
                                user,
                                path,
                                value.clone(),
                                operation.op.eq_ignore_ascii_case("add"),
                            )?;
                        }
                    }
                },
                "remove" => {
                    let path = operation
                        .path
                        .as_deref()
                        .ok_or_else(|| Error::invalid("noTarget", "A remove path is required"))?;
                    update(&mut profile, user, path, Value::Null, false)?;
                }
                _ => {
                    return Err(Error::invalid(
                        "invalidSyntax",
                        "Unsupported patch operation",
                    ));
                }
            }
        }
        profile.validate()?;
        Ok(profile)
    }
}

fn update(
    profile: &mut Profile,
    user: &User,
    path: &str,
    value: Value,
    append: bool,
) -> Result<()> {
    let path = path.to_ascii_lowercase();
    let prefix = USER.to_ascii_lowercase();
    let path = path
        .strip_prefix(&prefix)
        .and_then(|path| path.strip_prefix(':'))
        .unwrap_or(&path);
    let invalid = |_| Error::invalid("invalidValue", "Invalid SCIM patch value");
    match path {
        "active" => profile.active = serde_json::from_value(value).map_err(invalid)?,
        "username" => profile.user_name = serde_json::from_value(value).map_err(invalid)?,
        "displayname" => profile.display_name = serde_json::from_value(value).map_err(invalid)?,
        "name" => profile.name = serde_json::from_value(value).map_err(invalid)?,
        "emails" if value.is_null() => profile.emails.clear(),
        "emails" => {
            let mut emails = serde_json::from_value::<Vec<Email>>(value).map_err(invalid)?;
            if append {
                for email in emails {
                    if email.primary == Some(true) {
                        for existing in &mut profile.emails {
                            existing.primary = Some(false);
                        }
                    }
                    profile.emails.push(email);
                }
            } else {
                std::mem::swap(&mut profile.emails, &mut emails);
            }
        }
        "externalid" if value.as_str() == Some(&user.external_id) => {}
        "id" | "externalid" | "meta" => {
            return Err(Error::invalid(
                "mutability",
                "The selected attribute is immutable",
            ));
        }
        // Passwords from IdP test payloads have no role in OIDC sign-in.
        "password" => {}
        _ => {
            return Err(Error::invalid(
                "invalidPath",
                "Unsupported SCIM attribute path",
            ));
        }
    }
    Ok(())
}
