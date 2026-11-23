use super::{ApiError, Kind, Repository, Syntax, display_path};
use regex::{Regex, RegexBuilder};
use uuid::Uuid;

const MAX_QUERY_CHARS: usize = 512;

pub(super) enum Term {
    Literal(String),
    Expression(String),
}

impl Term {
    fn parse(value: &str) -> Result<Self, ApiError> {
        if let Some(pattern) = regex_inner(value) {
            if pattern.is_empty() {
                return Err(ApiError::Input(
                    "Enter a pattern between the regex delimiters",
                ));
            }
            return Ok(Self::Expression(pattern.to_owned()));
        }
        Ok(Self::Literal(match quoted_inner(value) {
            Some(inner) => unescape(inner),
            None if value.starts_with('"') => {
                return Err(ApiError::Input("Close the quoted code search qualifier"));
            }
            None => value.to_owned(),
        }))
    }

    fn scoped_value(self) -> Result<String, ApiError> {
        match self {
            Self::Literal(value) if !value.is_empty() => Ok(value),
            Self::Literal(_) => Err(ApiError::Input(
                "Enter a value for each code search qualifier",
            )),
            Self::Expression(_) => Err(ApiError::Input(
                "Use an exact repository, organization, or language name",
            )),
        }
    }
}

pub(super) fn compile_expression(pattern: &str, case_sensitive: bool) -> Result<Regex, ApiError> {
    RegexBuilder::new(pattern)
        .case_insensitive(!case_sensitive)
        .size_limit(256 * 1024)
        .dfa_size_limit(1024 * 1024)
        .nest_limit(32)
        .build()
        .map_err(|_| {
            ApiError::Input("Use a valid regular expression with a supported size and syntax")
        })
}

#[derive(Default)]
pub(super) struct Filters {
    repository: Option<(String, String)>,
    organization: Option<String>,
    language: Option<Language>,
    paths: Vec<Regex>,
    pub(super) regex_paths: bool,
}

impl Filters {
    pub(super) fn parse(query: &str, mut kind: Kind) -> Result<(Term, Kind, Self), ApiError> {
        if query.chars().count() > MAX_QUERY_CHARS || query.chars().any(char::is_control) {
            return Err(ApiError::Input(
                "Code search queries may contain up to 512 characters",
            ));
        }
        let mut rest = query;
        let mut literal = String::new();
        let mut scoped_text = None;
        let mut filters = Self::default();
        let mut qualifier_between_terms = false;
        loop {
            let word = rest.trim_start();
            if word.is_empty() {
                break;
            }
            let spaces = &rest[..rest.len() - word.len()];
            let (token, after) = word.split_at(word_end(word));
            rest = after;
            let qualifier = token.split_once(':').and_then(|(key, raw)| {
                let field = match key {
                    "repo" => Field::Repository,
                    "org" => Field::Organization,
                    "language" => Field::Language,
                    "path" => Field::Path,
                    "content" => Field::Text(Kind::Content),
                    "symbol" => Field::Text(Kind::Symbol),
                    _ => return None,
                };
                Some((field, raw))
            });
            let Some((field, raw)) = qualifier else {
                if regex_inner(token).is_some() {
                    single(&mut scoped_text, Term::parse(token)?)?;
                    continue;
                }
                if !literal.is_empty() {
                    literal.push_str(if qualifier_between_terms { " " } else { spaces });
                }
                literal.push_str(token);
                qualifier_between_terms = false;
                continue;
            };
            qualifier_between_terms = true;
            let value = Term::parse(raw)?;
            if matches!(&value, Term::Literal(value) if value.is_empty()) {
                return Err(ApiError::Input(
                    "Enter a value for each code search qualifier",
                ));
            }
            match field {
                Field::Repository => {
                    let value = value.scoped_value()?;
                    let (owner, name) = value
                        .split_once('/')
                        .filter(|(owner, name)| path_segment(owner) && path_segment(name))
                        .ok_or(ApiError::Input("Use repo:owner/name to filter code search"))?;
                    single(&mut filters.repository, (owner.to_owned(), name.to_owned()))?;
                }
                Field::Organization => {
                    let value = value.scoped_value()?;
                    if !path_segment(&value) {
                        return Err(ApiError::Input(
                            "Use the organization namespace or ID with org:",
                        ));
                    }
                    single(&mut filters.organization, value)?;
                }
                Field::Language => single(
                    &mut filters.language,
                    Language::parse(&value.scoped_value()?)?,
                )?,
                Field::Path => match value {
                    Term::Literal(value) => filters
                        .paths
                        .push(path_matcher(&value, raw.starts_with('"'))?),
                    Term::Expression(pattern) => {
                        filters.paths.push(compile_expression(&pattern, false)?);
                        filters.regex_paths = true;
                    }
                },
                Field::Text(scope) => {
                    kind = scope;
                    single(&mut scoped_text, value)?;
                }
            }
        }
        let term = if let Some(scoped_text) = scoped_text {
            if !literal.is_empty() {
                return Err(ApiError::Input(
                    "Use one content, symbol, or regular expression term",
                ));
            }
            scoped_text
        } else if let Some(inner) = quoted_inner(&literal) {
            Term::Literal(unescape(inner))
        } else {
            Term::Literal(literal)
        };
        if matches!(&term, Term::Literal(value) if value.is_empty()) {
            if filters.repository.is_none()
                && filters.organization.is_none()
                && filters.language.is_none()
                && filters.paths.is_empty()
            {
                return Err(ApiError::Input(
                    "Enter search text or a code search qualifier",
                ));
            }
            kind = Kind::Path;
        }
        Ok((term, kind, filters))
    }

    pub(super) fn repository_matches(&self, repo: &Repository) -> bool {
        self.repository.as_ref().is_none_or(|(owner, name)| {
            repo.config.owner.eq_ignore_ascii_case(owner)
                && repo.config.name.eq_ignore_ascii_case(name)
        }) && self.organization.as_ref().is_none_or(|org| {
            repo.organization.is_some_and(|id| {
                repo.config.owner.eq_ignore_ascii_case(org)
                    || Uuid::parse_str(org).is_ok_and(|expected| expected == id)
            })
        })
    }

    pub(super) fn file_matches(&self, path: &[u8]) -> bool {
        self.language.is_none_or(|language| language.matches(path))
            && (self.paths.is_empty() || {
                let path = display_path(path);
                self.paths.iter().all(|matcher| matcher.is_match(&path))
            })
    }
}

enum Field {
    Repository,
    Organization,
    Language,
    Path,
    Text(Kind),
}

fn single<T>(slot: &mut Option<T>, value: T) -> Result<(), ApiError> {
    if slot.is_some() {
        return Err(ApiError::Input(
            "Use one value per code search qualifier, except path:",
        ));
    }
    *slot = Some(value);
    Ok(())
}

fn path_segment(value: &str) -> bool {
    !matches!(value, "" | "." | "..")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}

fn word_end(value: &str) -> usize {
    let prefix = ["path:", "content:", "symbol:"]
        .iter()
        .find(|prefix| value.starts_with(**prefix))
        .map_or(0, |prefix| prefix.len());
    if let Some(end) = regex_end(&value[prefix..]) {
        return prefix + end;
    }
    let mut quoted = false;
    let mut characters = value.char_indices().peekable();
    while let Some((index, character)) = characters.next() {
        match character {
            '\\' if matches!(characters.peek(), Some((_, '"' | '\\'))) => {
                characters.next();
            }
            '"' => quoted = !quoted,
            character if !quoted && character.is_whitespace() => return index,
            _ => {}
        }
    }
    value.len()
}

fn regex_end(value: &str) -> Option<usize> {
    let mut characters = value.strip_prefix('/')?.char_indices();
    while let Some((index, character)) = characters.next() {
        match character {
            '\\' => {
                characters.next();
            }
            '/' => {
                let end = index + 2;
                return value[end..]
                    .chars()
                    .next()
                    .is_none_or(char::is_whitespace)
                    .then_some(end);
            }
            _ => {}
        }
    }
    None
}

fn regex_inner(value: &str) -> Option<&str> {
    (regex_end(value)? == value.len()).then(|| &value[1..value.len() - 1])
}

fn quoted_inner(value: &str) -> Option<&str> {
    let inner = value.strip_prefix('"')?;
    let mut characters = inner.char_indices().peekable();
    while let Some((index, character)) = characters.next() {
        match character {
            '\\' if matches!(characters.peek(), Some((_, '"' | '\\'))) => {
                characters.next();
            }
            '"' => return (index + 1 == inner.len()).then_some(&inner[..index]),
            _ => {}
        }
    }
    None
}

fn unescape(value: &str) -> String {
    let mut result = String::new();
    let mut characters = value.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\\' && matches!(characters.peek(), Some('"' | '\\')) {
            result.extend(characters.next());
        } else {
            result.push(character);
        }
    }
    result
}

fn path_matcher(value: &str, quoted: bool) -> Result<Regex, ApiError> {
    let anchored = !quoted && value.starts_with('/');
    let value = if anchored { &value[1..] } else { value };
    let glob = !quoted && value.contains(['*', '?']);
    let mut pattern = String::from(if anchored {
        "^"
    } else if glob {
        "(^|/)"
    } else {
        ""
    });
    if glob {
        let mut characters = value.chars().peekable();
        while let Some(character) = characters.next() {
            match character {
                '*' if characters.next_if_eq(&'*').is_some() => {
                    pattern.push_str(if characters.next_if_eq(&'/').is_some() {
                        "(?:.*/)?"
                    } else {
                        ".*"
                    });
                }
                '*' => pattern.push_str("[^/]*"),
                '?' => pattern.push_str("[^/]"),
                _ => pattern.push_str(&regex::escape(character.encode_utf8(&mut [0; 4]))),
            }
        }
        pattern.push('$');
    } else {
        pattern.push_str(&regex::escape(value));
    }
    RegexBuilder::new(&pattern)
        .case_insensitive(true)
        .dot_matches_new_line(true)
        .build()
        .map_err(|_| ApiError::Input("Invalid path: filter"))
}

#[derive(Clone, Copy)]
enum Language {
    Parsed(Syntax),
    Extensions(&'static [&'static [u8]]),
}

impl Language {
    fn parse(value: &str) -> Result<Self, ApiError> {
        Ok(match value.to_ascii_lowercase().as_str() {
            "c" => Self::Parsed(Syntax::C),
            "c++" => Self::Parsed(Syntax::Cpp),
            "go" => Self::Parsed(Syntax::Go),
            "java" => Self::Parsed(Syntax::Java),
            "javascript" => Self::Parsed(Syntax::JavaScript),
            "python" => Self::Parsed(Syntax::Python),
            "rust" => Self::Parsed(Syntax::Rust),
            "typescript" => Self::Parsed(Syntax::TypeScript),
            "c#" => Self::Extensions(&[b"cs"]),
            "css" => Self::Extensions(&[b"css"]),
            "html" => Self::Extensions(&[b"html", b"htm"]),
            "json" => Self::Extensions(&[b"json", b"jsonc"]),
            "kotlin" => Self::Extensions(&[b"kt", b"kts"]),
            "markdown" => Self::Extensions(&[b"md", b"markdown"]),
            "php" => Self::Extensions(&[b"php"]),
            "ruby" => Self::Extensions(&[b"rb"]),
            "shell" => Self::Extensions(&[b"sh", b"bash"]),
            "sql" => Self::Extensions(&[b"sql"]),
            "swift" => Self::Extensions(&[b"swift"]),
            "toml" => Self::Extensions(&[b"toml"]),
            "xml" => Self::Extensions(&[b"xml"]),
            "yaml" => Self::Extensions(&[b"yaml", b"yml"]),
            _ => {
                return Err(ApiError::Input(
                    "That language is not available in code search",
                ));
            }
        })
    }

    fn matches(self, path: &[u8]) -> bool {
        match self {
            Self::Parsed(language) => Syntax::for_path(path).is_some_and(|syntax| {
                syntax == language || (language == Syntax::TypeScript && syntax == Syntax::Tsx)
            }),
            Self::Extensions(extensions) => path
                .rsplit(|byte| *byte == b'/')
                .next()
                .and_then(|name| {
                    name.iter()
                        .rposition(|byte| *byte == b'.')
                        .map(|dot| &name[dot + 1..])
                })
                .is_some_and(|extension| {
                    extensions
                        .iter()
                        .any(|known| extension.eq_ignore_ascii_case(known))
                }),
        }
    }
}
