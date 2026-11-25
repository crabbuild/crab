use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Binding {
    pub name: String,
    pub identity: Option<[u8; 32]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<uuid::Uuid>,
}

pub(crate) fn identity(issuer: &str, subject: &str) -> crate::cells::Result<[u8; 32]> {
    // Length-delimited JSON preserves the qualified pair without concatenation
    // collisions. This derived key is versioned with the discussion index.
    Ok(*blake3::hash(&serde_json::to_vec(&(issuer, subject))?).as_bytes())
}

pub(crate) fn name(value: &str) -> Result<String, &'static str> {
    let value = value.to_ascii_lowercase();
    if value == "@me" {
        return Ok(value);
    }
    crate::cells::directory::validate_handle(&value)?;
    Ok(value)
}

impl super::Query {
    pub(crate) fn people(&self) -> std::collections::BTreeSet<String> {
        self.names(|term| match term {
            super::Term::Author(name)
            | super::Term::Commenter(name)
            | super::Term::Reviewer { name, .. } => Some(name),
            _ => None,
        })
    }

    pub(super) fn validate_people(&self, bindings: &[Binding]) -> Result<(), &'static str> {
        let names = self.people();
        let team_users = self.review_team_users();
        if bindings.len() != names.len()
            || !bindings
                .iter()
                .map(|binding| &binding.name)
                .eq(names.iter())
            || bindings.iter().any(|binding| {
                (binding.name == "@me" && binding.identity.is_none())
                    || binding.account.is_some_and(|account| {
                        account.is_nil()
                            || binding.identity.is_none()
                            || binding.name == "@me"
                            || !team_users.contains(&binding.name)
                    })
            })
        {
            return Err("People filters require ordered, resolved qualified identity bindings");
        }
        Ok(())
    }
}
