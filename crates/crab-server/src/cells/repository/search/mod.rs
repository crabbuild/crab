//! Shared issue and pull-request search language.

use super::collaboration::{SubjectKind, SubjectState};

pub(crate) mod assignees;
pub(crate) mod authors;
pub(crate) mod completion;
pub(crate) mod contributions;
mod counts;
pub(crate) mod index;
pub(crate) mod labels;
pub(crate) mod milestones;
mod navigation;
pub(crate) mod people;
pub(crate) mod reviews;
mod text;
pub(crate) use navigation::Choice;
pub(crate) mod results;

const MAX_TERMS: usize = 64;
const MAX_DEPTH: usize = 5;

#[derive(Debug)]
pub(crate) struct Query {
    expression: Expression,
}

#[derive(Clone, Debug, serde::Serialize)]
enum Expression {
    All,
    Term {
        value: Term,
        // Editing preserves spelling; cursor identity uses normalized values.
        #[serde(skip_serializing)]
        source: String,
    },
    Not(Box<Self>),
    And(Vec<Self>),
    Or(Vec<Self>),
    Scope {
        fields: text::Fields,
        expression: Box<Self>,
    },
}

#[derive(Clone, Debug, serde::Serialize)]
enum Term {
    Author(String),
    Commenter(String),
    Assignee(String),
    Reviewer { kind: reviews::Kind, name: String },
    ReviewStatus(reviews::status::Status),
    Count(counts::Filter),
    Text(String),
    Label(Vec<String>),
    Milestone(String),
    State(SubjectState),
    Reason(super::issues::lifecycle::Reason),
    Kind(SubjectKind),
    Unmerged,
    Draft(bool),
    Missing(Metadata),
}

#[derive(Clone, Debug, serde::Serialize)]
enum Metadata {
    Reason,
    Label,
    Assignee,
    Milestone,
}

impl Query {
    fn names(&self, select: fn(&Term) -> Option<&str>) -> std::collections::BTreeSet<String> {
        fn collect(
            expression: &Expression,
            select: fn(&Term) -> Option<&str>,
            names: &mut std::collections::BTreeSet<String>,
        ) {
            match expression {
                Expression::Term { value, .. } => {
                    if let Some(name) = select(value) {
                        names.insert(name.to_owned());
                    }
                }
                Expression::Not(inner)
                | Expression::Scope {
                    expression: inner, ..
                } => collect(inner, select, names),
                Expression::And(parts) | Expression::Or(parts) => {
                    for part in parts {
                        collect(part, select, names);
                    }
                }
                _ => (),
            }
        }
        let mut names = std::collections::BTreeSet::new();
        collect(&self.expression, select, &mut names);
        names
    }
    pub(super) fn needs_lifecycle(&self) -> bool {
        fn has(expression: &Expression) -> bool {
            match expression {
                Expression::Term {
                    value: Term::Reason(_) | Term::Missing(Metadata::Reason),
                    ..
                } => true,
                Expression::Not(inner)
                | Expression::Scope {
                    expression: inner, ..
                } => has(inner),
                Expression::And(parts) | Expression::Or(parts) => parts.iter().any(has),
                _ => false,
            }
        }
        has(&self.expression)
    }
    pub(crate) fn parse(source: Option<&str>) -> Result<Self, &'static str> {
        let Some(source) = source else {
            return Ok(Self {
                expression: Expression::All,
            });
        };
        if source.is_empty()
            || source.trim() != source
            || source.chars().count() > 256
            || source.chars().any(char::is_control)
        {
            return Err("Search query must contain 1–256 characters without controls");
        }
        let tokens = tokenize(source)?;
        let mut parser = Parser {
            tokens: &tokens,
            position: 0,
        };
        let expression = parser.or(0)?.finish();
        if parser.position != tokens.len() {
            return Err("Search query has an unexpected closing parenthesis");
        }
        Ok(Self { expression })
    }
}

#[derive(Debug, PartialEq)]
enum Token {
    Atom(String),
    And,
    Or,
    Open,
    Close,
}

fn tokenize(source: &str) -> Result<Vec<Token>, &'static str> {
    let mut characters = source.chars().peekable();
    let mut tokens = Vec::new();
    while let Some(character) = characters.next() {
        if character.is_whitespace() {
            continue;
        }
        if tokens.len() >= MAX_TERMS {
            return Err("Search query has too many terms");
        }
        match character {
            '(' => tokens.push(Token::Open),
            ')' => tokens.push(Token::Close),
            _ => {
                let mut atom = String::from(character);
                let mut quoted = character == '"';
                let mut escaped = false;
                while let Some(&next) = characters.peek() {
                    if !quoted && (next.is_whitespace() || next == '(' || next == ')') {
                        break;
                    }
                    characters.next();
                    atom.push(next);
                    if escaped {
                        escaped = false;
                    } else if quoted && next == '\\' {
                        escaped = true;
                    } else if next == '"' {
                        quoted = !quoted;
                    }
                }
                if quoted {
                    return Err("Search query has an unclosed quotation mark");
                }
                tokens.push(match atom.as_str() {
                    "AND" => Token::And,
                    "OR" => Token::Or,
                    _ => Token::Atom(atom),
                });
            }
        }
    }
    Ok(tokens)
}

struct Parser<'a> {
    tokens: &'a [Token],
    position: usize,
}

// Keep parentheses until field directives have been attached to their local
// conjunction. Flattening them earlier leaks a group's fields into its siblings.
enum Parsed {
    Atom(Expression),
    Group(Box<Self>),
    And(Vec<Self>),
    Or(Vec<Self>),
}
impl Parsed {
    fn finish(self) -> Expression {
        match self {
            Self::Atom(expression) => expression,
            Self::Group(inner) => inner.finish(),
            Self::Or(parts) => Expression::Or(parts.into_iter().map(Self::finish).collect()),
            Self::And(parts) => {
                let mut fields = None;
                let mut terms = Vec::new();
                for part in parts {
                    match part {
                        Self::Atom(Expression::Scope {
                            fields: selected,
                            expression,
                        }) if matches!(*expression, Expression::All) => {
                            fields =
                                Some(fields.map_or(selected, |current: text::Fields| {
                                    current.union(selected)
                                }));
                        }
                        other => terms.push(other.finish()),
                    }
                }
                let expression = match terms.len() {
                    0 => Expression::All,
                    1 => terms.remove(0),
                    _ => Expression::And(terms),
                };
                match fields {
                    Some(fields) => Expression::Scope {
                        fields,
                        expression: Box::new(expression),
                    },
                    None => expression,
                }
            }
        }
    }
}

impl Parser<'_> {
    fn or(&mut self, depth: usize) -> Result<Parsed, &'static str> {
        let mut parts = vec![self.and(depth)?];
        while self.tokens.get(self.position) == Some(&Token::Or) {
            self.position += 1;
            parts.push(self.and(depth)?);
        }
        Ok(if parts.len() == 1 {
            parts.remove(0)
        } else {
            Parsed::Or(parts)
        })
    }

    fn and(&mut self, depth: usize) -> Result<Parsed, &'static str> {
        let mut parts = vec![self.primary(depth)?];
        loop {
            match self.tokens.get(self.position) {
                None | Some(Token::Or | Token::Close) => break,
                Some(Token::And) => self.position += 1,
                _ => (),
            }
            parts.push(self.primary(depth)?);
        }
        Ok(if parts.len() == 1 {
            parts.remove(0)
        } else {
            Parsed::And(parts)
        })
    }

    fn primary(&mut self, depth: usize) -> Result<Parsed, &'static str> {
        let token = self
            .tokens
            .get(self.position)
            .ok_or("Search query needs a term after its operator")?;
        self.position += 1;
        match token {
            Token::Open => {
                if depth == MAX_DEPTH {
                    return Err("Search query supports at most five nested groups");
                }
                let inner = self.or(depth + 1)?;
                if self.tokens.get(self.position) != Some(&Token::Close) {
                    return Err("Search query has an unclosed parenthesis");
                }
                self.position += 1;
                Ok(Parsed::Group(Box::new(inner)))
            }
            Token::Atom(atom) => atom_expression(atom).map(Parsed::Atom),
            _ => Err("Search query needs a term between operators or parentheses"),
        }
    }
}

fn atom_expression(atom: &str) -> Result<Expression, &'static str> {
    let (negative, atom) = atom
        .strip_prefix('-')
        .map_or((false, atom), |atom| (true, atom));
    if atom.is_empty() {
        return Err("Search exclusion needs a term");
    }
    // Quoted phrases may contain colons; only an unquoted qualifier prefix
    // introduces metadata. Never reinterpret malformed qualifiers as free text.
    let qualifier = atom
        .split_once(':')
        .filter(|(prefix, _)| !prefix.contains('"'));
    if let Some((_, value)) =
        qualifier.filter(|(qualifier, _)| qualifier.eq_ignore_ascii_case("in"))
    {
        if negative {
            return Err("Search in qualifier cannot be negated; exclude a text term instead");
        }
        let fields = text::Fields::parse(&unquote(value)?)?;
        return Ok(Expression::Scope {
            fields,
            expression: Box::new(Expression::All),
        });
    }
    let term = match qualifier {
        None => Term::Text(unquote(atom)?.to_lowercase()),
        Some((qualifier, value)) => match qualifier.to_ascii_lowercase().as_str() {
            "author" => Term::Author(people::name(&unquote(value)?)?),
            "commenter" => Term::Commenter(people::name(&unquote(value)?)?),
            "assignee" => Term::Assignee(assignees::name(&unquote(value)?)?),
            "comments" | "reactions" | "interactions" => Term::Count(counts::Filter::parse(
                &qualifier.to_ascii_lowercase(),
                &unquote(value)?,
            )?),
            "review-requested" | "reviewed-by" | "review-involves" | "user-review-requested" => {
                Term::Reviewer {
                    kind: reviews::Kind::parse(&qualifier.to_ascii_lowercase())?,
                    name: people::name(&unquote(value)?)?,
                }
            }
            "review" => Term::ReviewStatus(reviews::status::Status::parse(&unquote(value)?)?),
            "label" => Term::Label(label_names(value)?),
            "reason" => Term::Reason(match unquote(value)?.to_lowercase().as_str() {
                "completed" => super::issues::lifecycle::Reason::Completed,
                "not planned" => super::issues::lifecycle::Reason::NotPlanned,
                "reopened" => super::issues::lifecycle::Reason::Reopened,
                _ => {
                    return Err("Search reason must be completed, quoted not planned, or reopened");
                }
            }),
            "milestone" => Term::Milestone(unquote(value)?.to_lowercase()),
            "is" | "state" => match unquote(value)?.to_ascii_lowercase().as_str() {
                "open" => Term::State(SubjectState::Open),
                "closed" => Term::State(SubjectState::Closed),
                "merged" if qualifier.eq_ignore_ascii_case("is") => {
                    Term::State(SubjectState::Merged)
                }
                "unmerged" if qualifier.eq_ignore_ascii_case("is") => Term::Unmerged,
                "issue" if qualifier.eq_ignore_ascii_case("is") => Term::Kind(SubjectKind::Issue),
                "pr" if qualifier.eq_ignore_ascii_case("is") => Term::Kind(SubjectKind::Pull),
                _ => return Err("Search is/state qualifier has an unsupported value"),
            },
            "draft" => match unquote(value)?.to_ascii_lowercase().as_str() {
                "true" => Term::Draft(true),
                "false" => Term::Draft(false),
                _ => return Err("Search draft qualifier must be true or false"),
            },
            "no" => {
                if negative {
                    return Err("Search no qualifier cannot be negated");
                }
                match unquote(value)?.to_ascii_lowercase().as_str() {
                    "label" => Term::Missing(Metadata::Label),
                    "assignee" => Term::Missing(Metadata::Assignee),
                    "milestone" => Term::Missing(Metadata::Milestone),
                    "reason" => Term::Missing(Metadata::Reason),
                    _ => {
                        return Err(
                            "Search no qualifier supports label, assignee, milestone and reason",
                        );
                    }
                }
            }
            _ => {
                return Err("Unsupported discussion search qualifier");
            }
        },
    };
    let expression = Expression::Term {
        value: term,
        source: atom.to_owned(),
    };
    Ok(if negative {
        Expression::Not(Box::new(expression))
    } else {
        expression
    })
}

fn unquote(value: &str) -> Result<String, &'static str> {
    if value.is_empty() {
        return Err("Search qualifier or phrase needs a value");
    }
    if !value.starts_with('"') {
        return (!value.contains('"'))
            .then(|| value.to_owned())
            .ok_or("Search quotation marks must enclose the whole value");
    }
    let inner = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .ok_or("Search query has an unclosed quotation mark")?;
    let mut result = String::new();
    let mut characters = inner.chars();
    while let Some(character) = characters.next() {
        match character {
            '"' => return Err("Search quotation marks must enclose the whole value"),
            '\\' => match characters.next() {
                Some(escaped @ ('"' | '\\')) => result.push(escaped),
                _ => {
                    return Err("Search quoted values only escape quotation marks and backslashes");
                }
            },
            _ => result.push(character),
        }
    }
    if result.trim().is_empty() {
        return Err("Search qualifier or phrase needs a value");
    }
    Ok(result)
}

fn label_names(value: &str) -> Result<Vec<String>, &'static str> {
    let mut quoted = false;
    let mut escaped = false;
    let mut start = 0;
    let mut names = Vec::new();
    for (offset, character) in value.char_indices() {
        if escaped {
            escaped = false;
        } else if quoted && character == '\\' {
            escaped = true;
        } else if character == '"' {
            quoted = !quoted;
        } else if character == ',' && !quoted {
            names.push(unquote(&value[start..offset])?.to_lowercase());
            start = offset + 1;
        }
    }
    names.push(unquote(&value[start..])?.to_lowercase());
    Ok(names)
}

#[cfg(test)]
mod tests;
