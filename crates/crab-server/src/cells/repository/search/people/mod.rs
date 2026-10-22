use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Binding {
    pub name: String,
    pub identity: Option<[u8; 32]>,
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

pub(super) fn names(
    query: &super::Query,
    select: fn(&super::Term) -> Option<&str>,
) -> std::collections::BTreeSet<String> {
    fn collect(
        expression: &super::Expression,
        select: fn(&super::Term) -> Option<&str>,
        names: &mut std::collections::BTreeSet<String>,
    ) {
        match expression {
            super::Expression::Term { value, .. } => {
                if let Some(name) = select(value) {
                    names.insert(name.to_owned());
                }
            }
            super::Expression::Not(inner) => collect(inner, select, names),
            super::Expression::And(parts) | super::Expression::Or(parts) => {
                for part in parts {
                    collect(part, select, names);
                }
            }
            _ => (),
        }
    }
    let mut names = std::collections::BTreeSet::new();
    collect(&query.expression, select, &mut names);
    names
}
pub(super) fn validate(
    names: &std::collections::BTreeSet<String>,
    bindings: &[Binding],
) -> Result<(), &'static str> {
    if bindings.len() != names.len()
        || !bindings
            .iter()
            .map(|binding| &binding.name)
            .eq(names.iter())
        || bindings
            .iter()
            .any(|binding| binding.name == "@me" && binding.identity.is_none())
    {
        return Err("People filters require ordered, resolved qualified identity bindings");
    }
    Ok(())
}
