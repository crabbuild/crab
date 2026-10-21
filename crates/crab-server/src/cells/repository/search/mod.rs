//! Shared issue and pull-request search language.

use super::collaboration::{SubjectKind, SubjectState};

pub(crate) mod completion;
pub(crate) mod index;
mod navigation;
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
}

#[derive(Clone, Debug, serde::Serialize)]
enum Term {
    Text(String),
    Label(Vec<String>),
    State(SubjectState),
    Kind(SubjectKind),
    Unmerged,
    Draft(bool),
    Missing(Metadata),
}

#[derive(Clone, Debug, serde::Serialize)]
enum Metadata {
    Label,
    Assignee,
}

impl Query {
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
        let expression = parser.or(0)?;
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

impl Parser<'_> {
    fn or(&mut self, depth: usize) -> Result<Expression, &'static str> {
        let mut parts = vec![self.and(depth)?];
        while self.tokens.get(self.position) == Some(&Token::Or) {
            self.position += 1;
            parts.push(self.and(depth)?);
        }
        Ok(if parts.len() == 1 {
            parts.remove(0)
        } else {
            Expression::Or(parts)
        })
    }

    fn and(&mut self, depth: usize) -> Result<Expression, &'static str> {
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
            Expression::And(parts)
        })
    }

    fn primary(&mut self, depth: usize) -> Result<Expression, &'static str> {
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
                Ok(inner)
            }
            Token::Atom(atom) => atom_expression(atom),
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
    let term = match qualifier {
        None => Term::Text(unquote(atom)?.to_lowercase()),
        Some((qualifier, value)) => match qualifier.to_ascii_lowercase().as_str() {
            "label" => Term::Label(label_names(value)?),
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
                    _ => return Err("Search no qualifier currently supports label and assignee"),
                }
            }
            _ => return Err("Unsupported search qualifier; use is, state, draft, label, or no"),
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
