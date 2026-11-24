use super::*;
use crate::auth::Identity;
use crate::catalog::topics::valid_topic;
use crate::cells::organization::properties::search::{MAX_TERMS, Term};

pub(super) struct Query {
    pub text: String,
    pub terms: Vec<Term>,
    pub archived: Option<bool>,
    pub fork: Option<bool>,
    topics: Vec<(String, bool)>,
    topic_counts: Vec<(u16, u16, bool)>,
}

const INVALID_TERM: &str = "Use props.NAME:VALUE and quote values containing spaces";
const INVALID_CURSOR: &str = "Invalid repository search cursor";

impl Query {
    pub(super) fn parse(mut source: &str, organization: Option<Uuid>) -> Result<Self> {
        let mut text = Vec::new();
        let mut terms = Vec::new();
        let mut archived = None;
        let mut fork = None;
        let mut topics = Vec::new();
        let mut topic_counts = Vec::new();
        while !source.trim_start().is_empty() {
            source = source.trim_start();
            let boundary = source.find(char::is_whitespace).unwrap_or(source.len());
            let word = &source[..boundary];
            let end = &source[boundary..];
            let (exclude, qualified) = word
                .strip_prefix('-')
                .map_or((false, word), |term| (true, term));
            if let Some((name, value)) = qualified.split_once(':') {
                if name.eq_ignore_ascii_case("topic") {
                    let value = value.to_ascii_lowercase();
                    if !valid_topic(&value) {
                        return Err(Error::Invalid(
                            "Use topic:NAME with letters, digits, or hyphens",
                        ));
                    }
                    topics.push((value, exclude));
                    source = end;
                    continue;
                }
                if name.eq_ignore_ascii_case("topics") {
                    let (min, max) = count_range(value)
                        .ok_or(Error::Invalid("Use topics:N, topics:>N, or topics:N..M"))?;
                    topic_counts.push((min, max, exclude));
                    source = end;
                    continue;
                }
                if name.eq_ignore_ascii_case("archived") {
                    let value = if value.eq_ignore_ascii_case("true") {
                        true
                    } else if value.eq_ignore_ascii_case("false") {
                        false
                    } else {
                        return Err(Error::Invalid("Use archived:true or archived:false"));
                    };
                    require_value(&mut archived, value != exclude)?;
                    source = end;
                    continue;
                }
                if name.eq_ignore_ascii_case("fork") {
                    if value.eq_ignore_ascii_case("only") {
                        require_value(&mut fork, !exclude)?;
                    } else if value.eq_ignore_ascii_case("false") {
                        require_value(&mut fork, exclude)?;
                    } else if !value.eq_ignore_ascii_case("true") || exclude {
                        return Err(Error::Invalid("Use fork:only, fork:false, or fork:true"));
                    }
                    source = end;
                    continue;
                }
            }
            if !qualified
                .get(..6)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("props."))
            {
                text.push(word);
                source = end;
                continue;
            }
            if organization.is_none() {
                return Err(Error::Invalid(
                    "Search by property from an organization's Repositories tab",
                ));
            }
            let (name, value) = qualified[6..]
                .split_once(':')
                .ok_or(Error::Invalid(INVALID_TERM))?;
            if name.is_empty()
                || name.len() > 75
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_-#$".contains(&byte))
            {
                return Err(Error::Invalid(INVALID_TERM));
            }
            let (value, remaining) = if value.starts_with('"') {
                let start = boundary - value.len() + 1;
                let (value, remaining) = source[start..]
                    .split_once('"')
                    .ok_or(Error::Invalid(INVALID_TERM))?;
                if remaining
                    .chars()
                    .next()
                    .is_some_and(|ch| !ch.is_whitespace())
                {
                    return Err(Error::Invalid(INVALID_TERM));
                }
                (value, remaining)
            } else {
                (value, end)
            };
            if value.is_empty()
                || value.len() > 75
                || value.trim() != value
                || value
                    .bytes()
                    .any(|byte| !(b' '..=b'~').contains(&byte) || byte == b'"')
            {
                return Err(Error::Invalid(INVALID_TERM));
            }
            terms.push(Term {
                name: name.to_ascii_lowercase(),
                value: value.into(),
                exclude,
            });
            if terms.len() > MAX_TERMS {
                return Err(Error::Invalid(
                    "Repository search accepts at most eight property filters",
                ));
            }
            source = remaining;
        }
        Ok(Self {
            text: text.join(" ").to_lowercase(),
            terms,
            archived,
            fork,
            topics,
            topic_counts,
        })
    }

    pub(super) fn filtered(&self) -> bool {
        !self.terms.is_empty()
            || self.archived.is_some()
            || self.fork.is_some()
            || !self.topics.is_empty()
            || !self.topic_counts.is_empty()
    }

    pub(super) fn matches_topics(&self, names: &[String]) -> bool {
        self.topics
            .iter()
            .all(|(name, exclude)| names.binary_search(name).is_ok() != *exclude)
            && self.topic_counts.iter().all(|&(min, max, exclude)| {
                (usize::from(min)..=usize::from(max)).contains(&names.len()) != exclude
            })
    }

    pub(super) fn cursor_key(
        &self,
        server: &Server,
        actor: &Identity,
        scope: &Scope,
    ) -> blake3::Hasher {
        let mut digest = blake3::Hasher::new_keyed(&server.cursor_key);
        digest.update(b"crab-server.repository-browse.v1");
        for value in [
            actor.issuer.as_bytes(),
            actor.subject.as_bytes(),
            self.text.as_bytes(),
        ] {
            hash_field(&mut digest, value);
        }
        digest.update(scope.organization().unwrap_or_default().as_bytes());
        match scope {
            Scope::Personal(owner) => {
                digest.update(b"personal\0");
                hash_field(&mut digest, owner.as_bytes());
            }
            Scope::Team { team, .. } => {
                digest.update(b"team\0");
                digest.update(team.as_bytes());
            }
            _ => {}
        }
        for term in &self.terms {
            hash_field(&mut digest, term.name.as_bytes());
            hash_field(&mut digest, term.value.as_bytes());
            digest.update(&[u8::from(term.exclude)]);
        }
        if self.archived.is_some() || self.fork.is_some() {
            digest.update(b"crab-server.repository-state.v1");
            digest.update(&[state(self.archived), state(self.fork)]);
        }
        if !self.topics.is_empty() || !self.topic_counts.is_empty() {
            digest.update(b"crab-server.repository-topics.v1");
            digest.update(&(self.topics.len() as u64).to_le_bytes());
            for (name, exclude) in &self.topics {
                hash_field(&mut digest, name.as_bytes());
                digest.update(&[u8::from(*exclude)]);
            }
            for &(min, max, exclude) in &self.topic_counts {
                digest.update(&min.to_le_bytes());
                digest.update(&max.to_le_bytes());
                digest.update(&[u8::from(exclude)]);
            }
        }
        digest
    }
}

fn count_range(value: &str) -> Option<(u16, u16)> {
    let number = |digits: &str| {
        digits
            .bytes()
            .all(|byte| byte.is_ascii_digit())
            .then(|| digits.parse::<u16>().ok())
            .flatten()
    };
    if let Some((min, max)) = value.split_once("..") {
        let (min, max) = (number(min)?, number(max)?);
        return (min <= max).then_some((min, max));
    }
    if let Some(min) = value.strip_prefix(">=") {
        return Some((number(min)?, u16::MAX));
    }
    if let Some(max) = value.strip_prefix("<=") {
        return Some((0, number(max)?));
    }
    if let Some(min) = value.strip_prefix('>') {
        return Some((number(min)?.saturating_add(1), u16::MAX));
    }
    if let Some(max) = value.strip_prefix('<') {
        let max = number(max)?;
        return Some(if max == 0 { (1, 0) } else { (0, max - 1) });
    }
    let exact = number(value)?;
    Some((exact, exact))
}

fn state(value: Option<bool>) -> u8 {
    value.map_or(0, |value| u8::from(value) + 1)
}

fn require_value(filter: &mut Option<bool>, value: bool) -> Result<()> {
    if filter.is_some_and(|previous| previous != value) {
        return Err(Error::Invalid(
            "Choose one value for each repository state filter",
        ));
    }
    *filter = Some(value);
    Ok(())
}

fn hash_field(digest: &mut blake3::Hasher, value: &[u8]) {
    digest.update(&(value.len() as u64).to_le_bytes());
    digest.update(value);
}

pub(super) struct Cursor {
    pub id: Uuid,
    pub state: Option<blake3::Hash>,
}

impl Cursor {
    pub(super) fn decode(value: &str, digest: &blake3::Hasher, filtered: bool) -> Result<Self> {
        if value.len() != if filtered { 162 } else { 97 } {
            return Err(Error::Invalid(INVALID_CURSOR));
        }
        let (id, rest) = value
            .split_once('.')
            .ok_or(Error::Invalid(INVALID_CURSOR))?;
        let id = Uuid::parse_str(id).map_err(|_| Error::Invalid(INVALID_CURSOR))?;
        let (tag, state) = if filtered {
            let (tag, state) = rest.split_once('.').ok_or(Error::Invalid(INVALID_CURSOR))?;
            (
                tag,
                Some(blake3::Hash::from_hex(state).map_err(|_| Error::Invalid(INVALID_CURSOR))?),
            )
        } else {
            (rest, None)
        };
        let cursor = Self { id, state };
        if cursor.tag(digest).to_hex().as_str() != tag {
            return Err(Error::Invalid(INVALID_CURSOR));
        }
        Ok(cursor)
    }

    pub(super) fn encode(&self, digest: &blake3::Hasher) -> String {
        let id = self.id.simple();
        let tag = self.tag(digest);
        match self.state {
            Some(state) => format!("{id}.{tag}.{state}"),
            None => format!("{id}.{tag}"),
        }
    }

    fn tag(&self, digest: &blake3::Hasher) -> blake3::Hash {
        let mut digest = digest.clone();
        digest.update(self.id.as_bytes());
        if let Some(state) = self.state {
            digest.update(state.as_bytes());
        }
        digest.finalize()
    }
}
