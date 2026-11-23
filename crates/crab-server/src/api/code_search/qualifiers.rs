use super::{ApiError, Kind, Repository, Syntax};
use regex::{Regex, RegexBuilder};
use regex_syntax::ParserBuilder;
use uuid::Uuid;

use crate::repository::code_index::QueryMask;

const MAX_QUERY_CHARS: usize = 512;
const EXPRESSION_NEST_LIMIT: u32 = 32;

enum Term {
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
            None => unescape(value),
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
        .nest_limit(EXPRESSION_NEST_LIMIT)
        .build()
        .map_err(|_| {
            ApiError::Input("Use a valid regular expression with a supported size and syntax")
        })
}

const MAX_TERMS: usize = 24;
const MAX_DEPTH: usize = 12;

pub(super) type Facts = [Truth; MAX_TERMS];

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Truth {
    No,
    Unknown,
    Yes,
}

impl Truth {
    pub(super) const fn from_bool(value: bool) -> Self {
        if value { Self::Yes } else { Self::No }
    }

    const fn invert(self) -> Self {
        match self {
            Self::Yes => Self::No,
            Self::No => Self::Yes,
            Self::Unknown => Self::Unknown,
        }
    }
}

enum Node {
    Term(usize),
    All(Vec<Self>),
    Any(Vec<Self>),
    Not(Box<Self>),
}

impl Node {
    fn evaluate(&self, facts: &[Truth], negate: bool) -> (Truth, u32) {
        match self {
            Self::Term(index) => {
                let value = facts[*index];
                if negate {
                    (value.invert(), 0)
                } else {
                    (value, u32::from(value == Truth::Yes) << *index)
                }
            }
            Self::Not(inner) => inner.evaluate(facts, !negate),
            Self::All(children) | Self::Any(children) => {
                let all = matches!(self, Self::All(_)) != negate;
                let mut value = Truth::from_bool(all);
                let mut evidence = 0;
                for child in children {
                    let (value_at_child, child_evidence) = child.evaluate(facts, negate);
                    match value_at_child {
                        Truth::No if all => return (Truth::No, 0),
                        Truth::Yes if !all => value = Truth::Yes,
                        Truth::Unknown if all || value == Truth::No => value = Truth::Unknown,
                        _ => (),
                    }
                    if value_at_child == Truth::Yes {
                        evidence |= child_evidence;
                    }
                }
                (value, evidence)
            }
        }
    }
}

pub(super) struct QueryPlan {
    root: Node,
    pub(super) leaves: Vec<Predicate>,
    pub(super) bounded: bool,
}

pub(super) enum Predicate {
    Repository(String, String),
    Organization(String),
    Language(Language),
    Path(Regex),
    Text {
        kind: Kind,
        matcher: Regex,
        filter: Option<Box<QueryMask>>,
    },
}

impl QueryPlan {
    pub(super) fn parse(query: &str, kind: Kind, case_sensitive: bool) -> Result<Self, ApiError> {
        if query.chars().count() > MAX_QUERY_CHARS || query.chars().any(char::is_control) {
            return Err(ApiError::Input(
                "Code search queries may contain up to 512 characters",
            ));
        }
        let mut tokens = Vec::new();
        let mut rest = query.trim_start();
        while !rest.is_empty() {
            let (token, tail) = rest.split_at(word_end(rest));
            tokens.push(token);
            rest = tail.trim_start();
        }
        let mut parser = Parser {
            tokens: tokens.into_iter().peekable(),
            kind,
            case_sensitive,
            leaves: Vec::new(),
            bounded: false,
        };
        let root = parser.or(0)?;
        if parser.tokens.next().is_some() {
            return Err(invalid_query());
        }
        Ok(Self {
            root,
            leaves: parser.leaves,
            bounded: parser.bounded,
        })
    }

    pub(super) fn evaluate(&self, facts: &[Truth]) -> (Truth, u32) {
        self.root.evaluate(facts, false)
    }

    pub(super) fn facts(&self, mut evaluate: impl FnMut(&Predicate) -> Truth) -> Facts {
        let mut facts = [Truth::Unknown; MAX_TERMS];
        for (term, fact) in self.leaves.iter().zip(&mut facts) {
            *fact = evaluate(term);
        }
        facts
    }
}

impl Predicate {
    fn parse(value: &str, kind: Kind, case_sensitive: bool) -> Result<(Self, bool), ApiError> {
        let (scope, raw) = value.split_once(':').unwrap_or(("", value));
        let raw = match scope {
            "repo" | "org" | "language" | "path" | "content" | "symbol" => raw,
            _ => value,
        };
        let term = Term::parse(raw)?;
        if matches!(&term, Term::Literal(value) if value.is_empty()) {
            return Err(ApiError::Input(
                "Enter search text or a code search qualifier value",
            ));
        }
        let expression = matches!(&term, Term::Expression(_));
        let predicate = match scope {
            "repo" => {
                let value = term.scoped_value()?;
                let (owner, name) = value
                    .split_once('/')
                    .filter(|(owner, name)| path_segment(owner) && path_segment(name))
                    .ok_or(ApiError::Input("Use repo:owner/name to filter code search"))?;
                Self::Repository(owner.to_owned(), name.to_owned())
            }
            "org" => {
                let value = term.scoped_value()?;
                if !path_segment(&value) {
                    return Err(ApiError::Input(
                        "Use the organization namespace or ID with org:",
                    ));
                }
                Self::Organization(value)
            }
            "language" => Self::Language(Language::parse(&term.scoped_value()?)?),
            "path" => Self::Path(match term {
                Term::Literal(value) => path_matcher(&value, raw.starts_with('"'))?,
                Term::Expression(pattern) => compile_expression(&pattern, case_sensitive)?,
            }),
            _ => {
                let kind = match scope {
                    "content" => Kind::Content,
                    "symbol" => Kind::Symbol,
                    _ => kind,
                };
                let (pattern, matcher) = match term {
                    Term::Literal(literal) => {
                        if literal.chars().count() > 128 {
                            return Err(ApiError::Input(
                                "Search terms may contain up to 128 characters",
                            ));
                        }
                        let pattern = regex::escape(&literal);
                        let matcher = RegexBuilder::new(&pattern)
                            .case_insensitive(!case_sensitive)
                            .build()
                            .map_err(|_| {
                                ApiError::Input("The search term could not be compiled")
                            })?;
                        (pattern, matcher)
                    }
                    Term::Expression(pattern) => {
                        let matcher = compile_expression(&pattern, case_sensitive)?;
                        (pattern, matcher)
                    }
                };
                let hir = ParserBuilder::new()
                    .case_insensitive(!case_sensitive)
                    .nest_limit(EXPRESSION_NEST_LIMIT)
                    .build()
                    .parse(&pattern)
                    .ok();
                Self::Text {
                    kind,
                    matcher,
                    filter: hir.as_ref().and_then(QueryMask::from_regex).map(Box::new),
                }
            }
        };
        Ok((predicate, expression))
    }

    pub(super) fn repository_fact(&self, repo: &Repository) -> Truth {
        match self {
            Self::Repository(owner, name) => Truth::from_bool(
                repo.config.owner.eq_ignore_ascii_case(owner)
                    && repo.config.name.eq_ignore_ascii_case(name),
            ),
            Self::Organization(org) => Truth::from_bool(repo.organization.is_some_and(|id| {
                repo.config.owner.eq_ignore_ascii_case(org)
                    || Uuid::parse_str(org).is_ok_and(|expected| expected == id)
            })),
            _ => Truth::Unknown,
        }
    }

    pub(super) fn file_fact(&self, path: &[u8], display: &str, repo: Truth) -> Truth {
        match self {
            Self::Language(language) => Truth::from_bool(language.matches(path)),
            Self::Path(matcher)
            | Self::Text {
                kind: Kind::Path,
                matcher,
                ..
            } => Truth::from_bool(matcher.is_match(display)),
            Self::Text {
                kind: Kind::Any,
                matcher,
                ..
            } if matcher.is_match(display) => Truth::Yes,
            Self::Text {
                kind: Kind::Symbol, ..
            } if Syntax::for_path(path).is_none() => Truth::No,
            _ => repo,
        }
    }

    pub(super) fn uses_text(&self) -> bool {
        matches!(self, Self::Text { kind, .. } if *kind != Kind::Path)
    }

    pub(super) fn uses_symbols(&self) -> bool {
        matches!(
            self,
            Self::Text {
                kind: Kind::Symbol,
                ..
            }
        )
    }
}

struct Parser<'a> {
    tokens: std::iter::Peekable<std::vec::IntoIter<&'a str>>,
    kind: Kind,
    case_sensitive: bool,
    leaves: Vec<Predicate>,
    bounded: bool,
}

fn invalid_query() -> ApiError {
    ApiError::Input("Enter a term after AND, OR, or NOT and close each parenthesized group")
}

impl Parser<'_> {
    fn or(&mut self, depth: usize) -> Result<Node, ApiError> {
        let mut first = self.and(depth)?;
        if self.tokens.next_if_eq(&"OR").is_some() {
            self.bounded = true;
            let mut children = vec![first, self.and(depth)?];
            while self.tokens.next_if_eq(&"OR").is_some() {
                children.push(self.and(depth)?);
            }
            first = Node::Any(children);
        }
        Ok(first)
    }

    fn and(&mut self, depth: usize) -> Result<Node, ApiError> {
        let mut first = self.unary(depth)?;
        if self
            .tokens
            .peek()
            .is_some_and(|token| !matches!(*token, "OR" | ")"))
        {
            self.bounded = true;
            let mut children = vec![first];
            while self
                .tokens
                .peek()
                .is_some_and(|token| !matches!(*token, "OR" | ")"))
            {
                self.tokens.next_if_eq(&"AND");
                children.push(self.unary(depth)?);
            }
            first = Node::All(children);
        }
        Ok(first)
    }

    fn unary(&mut self, depth: usize) -> Result<Node, ApiError> {
        if depth > MAX_DEPTH || self.leaves.len() == MAX_TERMS {
            return Err(ApiError::Input(
                "Limit code search to 24 terms and 12 nested operators",
            ));
        }
        match self.tokens.next() {
            Some("NOT") => {
                self.bounded = true;
                Ok(Node::Not(Box::new(self.unary(depth + 1)?)))
            }
            Some("(") => {
                let node = self.or(depth + 1)?;
                self.tokens.next_if_eq(&")").ok_or_else(invalid_query)?;
                Ok(node)
            }
            Some(token) if !matches!(token, "AND" | "OR" | ")") => {
                let (predicate, expression) =
                    Predicate::parse(token, self.kind, self.case_sensitive)?;
                let term = self.leaves.len();
                self.leaves.push(predicate);
                self.bounded |= expression;
                Ok(Node::Term(term))
            }
            _ => Err(invalid_query()),
        }
    }
}

fn path_segment(value: &str) -> bool {
    !matches!(value, "" | "." | "..")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}

fn token_boundary(character: char) -> bool {
    character.is_whitespace() || matches!(character, '(' | ')')
}

fn word_end(value: &str) -> usize {
    if value.starts_with(['(', ')']) {
        return 1;
    }
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
            character if !quoted && token_boundary(character) => return index,
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
                    .is_none_or(token_boundary)
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
pub(super) enum Language {
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
