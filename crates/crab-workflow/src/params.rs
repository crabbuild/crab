//! Workflow parameter scalar parsing contracts.
//!
//! A params or metrics document is YAML, JSON, TOML, or Python literal
//! assignments whose leaves are scalars: booleans, integers, floats, strings,
//! or null. Nested maps and arrays are flattened to dotted-key form so callers
//! can compare `ScalarMap` values without knowing the source format.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;

use crate::error::{Result, WorkflowError};

pub use crate::params_runtime::{
    RenderOptions, ScalarDiff, diff, find_git_dir, param_key_matches, read_at_ref,
    read_blob_at_ref, read_working_tree_files, render_json, render_markdown, render_pr_comment,
    render_table, resolve_stage_param_values, resolve_stage_param_values_with_wdir,
};

/// A single parameter or metric value.
///
/// The variant layout intentionally mirrors JSON's scalar types.
/// YAML and TOML numbers collapse onto [`Scalar::Int`] when the
/// value fits a signed 64-bit integer and [`Scalar::Float`]
/// otherwise; strings are never coerced to numbers.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Scalar {
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Null,
}

impl Scalar {
    /// Human-readable value, stable across renderers.
    ///
    /// Floats use Rust's default `{}` format; integers and booleans
    /// serialize to their canonical tokens. Strings are emitted
    /// unquoted so table renderers aren't forced to strip them.
    pub fn display(&self) -> String {
        match self {
            Self::Bool(b) => b.to_string(),
            Self::Int(i) => i.to_string(),
            Self::Float(f) => format_float(*f),
            Self::String(s) => s.clone(),
            Self::Null => "null".to_owned(),
        }
    }

    /// `Some(f64)` iff the scalar is [`Scalar::Int`] or
    /// [`Scalar::Float`]. Integers promote losslessly for values
    /// where `i64 → f64` is exact (anything within 2^53); callers
    /// that care about that boundary must check themselves.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Int(i) => Some(*i as f64),
            Self::Float(f) => Some(*f),
            _ => None,
        }
    }
}

fn format_float(f: f64) -> String {
    // `{}` on f64 keeps integer-valued floats as `3` rather than
    // `3.0`; we prefer the decimal dot so rendered tables don't
    // silently disguise floats as ints.
    if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e16 {
        format!("{f:.1}")
    } else {
        format!("{f}")
    }
}

/// Ordered map of dotted key → scalar. `BTreeMap` so iteration
/// and diff output are deterministic.
pub type ScalarMap = BTreeMap<String, Scalar>;

/// Parse by extension. Recognizes `.yaml` / `.yml` (YAML),
/// `.json` (JSON), `.toml` (TOML), and `.py` (literal Python
/// assignments). Unknown extensions fail with a
/// [`WorkflowError::ParamsInvalid`] so a misnamed file never gets
/// silently mis-parsed.
pub fn parse(bytes: &[u8], path: &Path) -> Result<ScalarMap> {
    let text = std::str::from_utf8(bytes).map_err(|e| WorkflowError::ParamsInvalid {
        key: format!("{}: file is not valid UTF-8 ({e})", path.display()),
        origin: "params".into(),
    })?;

    match path.extension().and_then(|s| s.to_str()) {
        Some("yaml" | "yml") => parse_yaml(text),
        Some("json") => parse_json(text),
        Some("toml") => parse_toml(text),
        Some("py") => parse_python(text),
        other => Err(WorkflowError::ParamsInvalid {
            key: format!(
                "{}: unsupported extension {:?} (expected .yaml, .yml, .json, .toml, or .py)",
                path.display(),
                other.unwrap_or("<none>")
            ),
            origin: "params".into(),
        }),
    }
}

/// Parse a YAML document into a flattened `ScalarMap`. The root
/// node must be a mapping.
pub fn parse_yaml(text: &str) -> Result<ScalarMap> {
    let root: serde_yaml::Value =
        serde_yaml::from_str(text).map_err(|e| WorkflowError::ParamsInvalid {
            key: format!("yaml parse error: {e}"),
            origin: "params".into(),
        })?;
    let mut out = ScalarMap::new();
    flatten_yaml(&root, "", &mut out)?;
    Ok(out)
}

/// Parse a JSON document into a flattened `ScalarMap`. The root
/// node must be an object.
pub fn parse_json(text: &str) -> Result<ScalarMap> {
    let root: serde_json::Value =
        serde_json::from_str(text).map_err(|e| WorkflowError::ParamsInvalid {
            key: format!("json parse error: {e}"),
            origin: "params".into(),
        })?;
    let mut out = ScalarMap::new();
    flatten_json(&root, "", &mut out)?;
    Ok(out)
}

/// Parse a TOML document into a flattened `ScalarMap`.
pub fn parse_toml(text: &str) -> Result<ScalarMap> {
    let root: toml::Value = toml::from_str(text).map_err(|e| WorkflowError::ParamsInvalid {
        key: format!("toml parse error: {e}"),
        origin: "params".into(),
    })?;
    let mut out = ScalarMap::new();
    flatten_toml(&root, "", &mut out)?;
    Ok(out)
}

/// Parse a Python params file without executing it.
///
/// DVC reads Python params through Python's AST and literal evaluator.
/// This mirrors the safe subset that matters for params files: literal
/// top-level assignments, class constants, and `self.*` assignments inside
/// `__init__`, including nested dict/list/tuple/set values and
/// `dict(key=value)` keyword construction. Dynamic expressions are ignored,
/// so a tracked key that depends on code execution is reported as missing
/// instead of executing arbitrary repository code.
pub fn parse_python(text: &str) -> Result<ScalarMap> {
    let mut out = ScalarMap::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();
        if line.len() != trimmed.len() {
            i += 1;
            continue;
        }

        if let Some(class_name) = parse_python_class_name(trimmed) {
            i = parse_python_class_block(&lines, i, class_name, &mut out)?;
            continue;
        }

        let Some((name, rhs)) = split_python_assignment(trimmed) else {
            i += 1;
            continue;
        };

        if let Some((value, next)) =
            parse_python_literal_from_lines(&lines, i + 1, rhs, lines.len())
        {
            flatten_python(&value, name, &mut out)?;
            i = next;
            continue;
        }
        i += 1;
    }

    Ok(out)
}

fn parse_python_class_block(
    lines: &[&str],
    class_line: usize,
    class_name: &str,
    out: &mut ScalarMap,
) -> Result<usize> {
    let class_indent = leading_indent(lines[class_line]);
    let end = python_block_end(lines, class_line + 1, class_indent);
    let body_indent = first_significant_indent(lines, class_line + 1, end);
    let mut i = class_line + 1;

    while i < end {
        if is_python_blank_or_comment(lines[i]) || Some(leading_indent(lines[i])) != body_indent {
            i += 1;
            continue;
        }

        let trimmed = lines[i].trim_start();
        if is_python_init_def(trimmed) {
            let def_indent = leading_indent(lines[i]);
            let def_end = python_block_end(lines, i + 1, def_indent);
            parse_python_init_body(lines, i + 1, def_end, class_name, out)?;
            i = def_end;
            continue;
        }

        let Some((name, rhs)) = split_python_assignment(trimmed) else {
            i += 1;
            continue;
        };
        if let Some((value, next)) = parse_python_literal_from_lines(lines, i + 1, rhs, end) {
            flatten_python(&value, &push_key(class_name, name), out)?;
            i = next;
        } else {
            i += 1;
        }
    }

    Ok(end)
}

fn parse_python_init_body(
    lines: &[&str],
    start: usize,
    end: usize,
    class_name: &str,
    out: &mut ScalarMap,
) -> Result<()> {
    let mut i = start;
    while i < end {
        if is_python_blank_or_comment(lines[i]) {
            i += 1;
            continue;
        }

        let trimmed = lines[i].trim_start();
        let Some((name, rhs)) = split_python_self_assignment(trimmed) else {
            i += 1;
            continue;
        };
        if let Some((value, next)) = parse_python_literal_from_lines(lines, i + 1, rhs, end) {
            flatten_python(&value, &push_key(class_name, name), out)?;
            i = next;
        } else {
            i += 1;
        }
    }
    Ok(())
}

fn parse_python_literal_from_lines(
    lines: &[&str],
    mut next: usize,
    rhs: &str,
    end: usize,
) -> Option<(PythonLiteral, usize)> {
    let mut expr = rhs.to_owned();
    loop {
        match parse_python_literal(&expr) {
            Ok(value) => return Some((value, next)),
            Err(PythonParseError::Incomplete) if next < end => {
                expr.push('\n');
                expr.push_str(lines[next]);
                next += 1;
            }
            Err(PythonParseError::Incomplete | PythonParseError::Unsupported) => return None,
        }
    }
}

fn push_key(prefix: &str, tail: &str) -> String {
    if prefix.is_empty() {
        tail.to_owned()
    } else {
        format!("{prefix}.{tail}")
    }
}

fn insert_scalar(map: &mut ScalarMap, key: String, value: Scalar) -> Result<()> {
    if let Scalar::Float(f) = value
        && !f.is_finite()
    {
        return Err(WorkflowError::ParamsInvalid {
            key: format!("non-finite float at '{key}' (NaN or Infinity)"),
            origin: "params".into(),
        });
    }
    map.insert(key, value);
    Ok(())
}

fn split_python_assignment(line: &str) -> Option<(&str, &str)> {
    let name_end = parse_ident_end(line, 0)?;
    let name = &line[..name_end];
    if is_python_keyword(name) {
        return None;
    }

    let mut pos = skip_inline_ws(line, name_end);
    if line[pos..].starts_with(':') {
        pos += 1;
        let eq = line[pos..].find('=')?;
        pos += eq;
    }

    if !line[pos..].starts_with('=') || line[pos..].starts_with("==") {
        return None;
    }
    Some((name, &line[pos + 1..]))
}

fn split_python_self_assignment(line: &str) -> Option<(&str, &str)> {
    let rest = line.strip_prefix("self.")?;
    let name_end = parse_ident_end(rest, 0)?;
    let name = &rest[..name_end];

    let mut pos = skip_inline_ws(rest, name_end);
    if rest[pos..].starts_with(':') {
        pos += 1;
        let eq = rest[pos..].find('=')?;
        pos += eq;
    }

    if !rest[pos..].starts_with('=') || rest[pos..].starts_with("==") {
        return None;
    }
    Some((name, &rest[pos + 1..]))
}

fn parse_python_class_name(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("class ")?;
    let name_end = parse_ident_end(rest, 0)?;
    let name = &rest[..name_end];
    let tail = rest[name_end..].trim_start();
    if tail.starts_with(':') || tail.starts_with('(') {
        Some(name)
    } else {
        None
    }
}

fn is_python_init_def(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("def") else {
        return false;
    };
    let rest = rest.trim_start();
    rest.starts_with("__init__(")
}

fn leading_indent(line: &str) -> usize {
    line.len() - line.trim_start_matches([' ', '\t']).len()
}

fn is_python_blank_or_comment(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.is_empty() || trimmed.starts_with('#')
}

fn python_block_end(lines: &[&str], start: usize, parent_indent: usize) -> usize {
    let mut idx = start;
    while idx < lines.len() {
        if is_python_blank_or_comment(lines[idx]) {
            idx += 1;
            continue;
        }
        if leading_indent(lines[idx]) <= parent_indent {
            break;
        }
        idx += 1;
    }
    idx
}

fn first_significant_indent(lines: &[&str], start: usize, end: usize) -> Option<usize> {
    lines
        .iter()
        .take(end)
        .skip(start)
        .find(|line| !is_python_blank_or_comment(line))
        .map(|line| leading_indent(line))
}

fn parse_ident_end(text: &str, start: usize) -> Option<usize> {
    let mut chars = text[start..].char_indices();
    let (_, first) = chars.next()?;
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return None;
    }
    let mut end = start + first.len_utf8();
    for (offset, ch) in chars {
        if ch == '_' || ch.is_ascii_alphanumeric() {
            end = start + offset + ch.len_utf8();
        } else {
            break;
        }
    }
    Some(end)
}

fn skip_inline_ws(text: &str, mut pos: usize) -> usize {
    while let Some(ch) = text[pos..].chars().next() {
        if ch != ' ' && ch != '\t' {
            break;
        }
        pos += ch.len_utf8();
    }
    pos
}

fn is_python_keyword(name: &str) -> bool {
    matches!(
        name,
        "False"
            | "None"
            | "True"
            | "and"
            | "as"
            | "assert"
            | "async"
            | "await"
            | "break"
            | "class"
            | "continue"
            | "def"
            | "del"
            | "elif"
            | "else"
            | "except"
            | "finally"
            | "for"
            | "from"
            | "global"
            | "if"
            | "import"
            | "in"
            | "is"
            | "lambda"
            | "nonlocal"
            | "not"
            | "or"
            | "pass"
            | "raise"
            | "return"
            | "try"
            | "while"
            | "with"
            | "yield"
    )
}

#[derive(Debug, Clone, PartialEq)]
pub enum PythonLiteral {
    Scalar(Scalar),
    Sequence(Vec<PythonLiteral>),
    Mapping(BTreeMap<String, PythonLiteral>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PythonParseError {
    Incomplete,
    Unsupported,
}

pub fn parse_python_literal(text: &str) -> std::result::Result<PythonLiteral, PythonParseError> {
    let mut parser = PythonLiteralParser::new(text);
    let value = parser.parse_value()?;
    parser.skip_ws_and_comments();
    if parser.is_eof() {
        Ok(value)
    } else {
        Err(PythonParseError::Unsupported)
    }
}

struct PythonLiteralParser<'a> {
    text: &'a str,
    pos: usize,
}

impl<'a> PythonLiteralParser<'a> {
    fn new(text: &'a str) -> Self {
        Self { text, pos: 0 }
    }

    fn parse_value(&mut self) -> std::result::Result<PythonLiteral, PythonParseError> {
        self.skip_ws_and_comments();
        match self.peek_char() {
            Some('{') => self.parse_brace(),
            Some('[') => self.parse_sequence('[', ']'),
            Some('(') => self.parse_tuple_or_group(),
            Some('"' | '\'') => self
                .parse_string()
                .map(|s| PythonLiteral::Scalar(Scalar::String(s))),
            Some('+' | '-' | '0'..='9' | '.') => self.parse_number(),
            Some(ch) if ch == '_' || ch.is_ascii_alphabetic() => self.parse_identifier_value(),
            Some(_) => Err(PythonParseError::Unsupported),
            None => Err(PythonParseError::Incomplete),
        }
    }

    fn parse_identifier_value(&mut self) -> std::result::Result<PythonLiteral, PythonParseError> {
        let ident = self.parse_identifier()?;
        match ident.as_str() {
            "True" => Ok(PythonLiteral::Scalar(Scalar::Bool(true))),
            "False" => Ok(PythonLiteral::Scalar(Scalar::Bool(false))),
            "None" => Ok(PythonLiteral::Scalar(Scalar::Null)),
            "dict" => self.parse_dict_call(),
            _ => Err(PythonParseError::Unsupported),
        }
    }

    fn parse_dict_call(&mut self) -> std::result::Result<PythonLiteral, PythonParseError> {
        self.skip_ws_and_comments();
        if !self.consume_char('(') {
            return Err(PythonParseError::Unsupported);
        }
        let mut map = BTreeMap::new();
        loop {
            self.skip_ws_and_comments();
            if self.consume_char(')') {
                break;
            }
            let key = self.parse_identifier()?;
            self.skip_ws_and_comments();
            if !self.consume_char('=') {
                return Err(PythonParseError::Unsupported);
            }
            let value = self.parse_value()?;
            map.insert(key, value);
            self.skip_ws_and_comments();
            if self.consume_char(')') {
                break;
            }
            if !self.consume_char(',') {
                return Err(if self.is_eof() {
                    PythonParseError::Incomplete
                } else {
                    PythonParseError::Unsupported
                });
            }
        }
        Ok(PythonLiteral::Mapping(map))
    }

    fn parse_brace(&mut self) -> std::result::Result<PythonLiteral, PythonParseError> {
        self.expect_char('{')?;
        self.skip_ws_and_comments();
        if self.consume_char('}') {
            return Ok(PythonLiteral::Mapping(BTreeMap::new()));
        }

        let first = self.parse_value()?;
        self.skip_ws_and_comments();
        if self.consume_char(':') {
            let mut map = BTreeMap::new();
            let key = literal_to_python_key(&first)?;
            let value = self.parse_value()?;
            map.insert(key, value);
            loop {
                self.skip_ws_and_comments();
                if self.consume_char('}') {
                    break;
                }
                if !self.consume_char(',') {
                    return Err(if self.is_eof() {
                        PythonParseError::Incomplete
                    } else {
                        PythonParseError::Unsupported
                    });
                }
                self.skip_ws_and_comments();
                if self.consume_char('}') {
                    break;
                }
                let key = self.parse_value()?;
                self.skip_ws_and_comments();
                if !self.consume_char(':') {
                    return Err(PythonParseError::Unsupported);
                }
                let value = self.parse_value()?;
                map.insert(literal_to_python_key(&key)?, value);
            }
            Ok(PythonLiteral::Mapping(map))
        } else {
            let mut values = vec![first];
            loop {
                self.skip_ws_and_comments();
                if self.consume_char('}') {
                    break;
                }
                if !self.consume_char(',') {
                    return Err(if self.is_eof() {
                        PythonParseError::Incomplete
                    } else {
                        PythonParseError::Unsupported
                    });
                }
                self.skip_ws_and_comments();
                if self.consume_char('}') {
                    break;
                }
                values.push(self.parse_value()?);
            }
            Ok(PythonLiteral::Sequence(values))
        }
    }

    fn parse_sequence(
        &mut self,
        open: char,
        close: char,
    ) -> std::result::Result<PythonLiteral, PythonParseError> {
        self.expect_char(open)?;
        let mut values = Vec::new();
        loop {
            self.skip_ws_and_comments();
            if self.consume_char(close) {
                break;
            }
            values.push(self.parse_value()?);
            self.skip_ws_and_comments();
            if self.consume_char(close) {
                break;
            }
            if !self.consume_char(',') {
                return Err(if self.is_eof() {
                    PythonParseError::Incomplete
                } else {
                    PythonParseError::Unsupported
                });
            }
        }
        Ok(PythonLiteral::Sequence(values))
    }

    fn parse_tuple_or_group(&mut self) -> std::result::Result<PythonLiteral, PythonParseError> {
        self.expect_char('(')?;
        self.skip_ws_and_comments();
        if self.consume_char(')') {
            return Ok(PythonLiteral::Sequence(Vec::new()));
        }

        let first = self.parse_value()?;
        self.skip_ws_and_comments();
        if self.consume_char(')') {
            return Ok(first);
        }

        if !self.consume_char(',') {
            return Err(if self.is_eof() {
                PythonParseError::Incomplete
            } else {
                PythonParseError::Unsupported
            });
        }
        let mut values = vec![first];
        loop {
            self.skip_ws_and_comments();
            if self.consume_char(')') {
                break;
            }
            values.push(self.parse_value()?);
            self.skip_ws_and_comments();
            if self.consume_char(')') {
                break;
            }
            if !self.consume_char(',') {
                return Err(if self.is_eof() {
                    PythonParseError::Incomplete
                } else {
                    PythonParseError::Unsupported
                });
            }
        }
        Ok(PythonLiteral::Sequence(values))
    }

    fn parse_number(&mut self) -> std::result::Result<PythonLiteral, PythonParseError> {
        let start = self.pos;
        if matches!(self.peek_char(), Some('+' | '-')) {
            self.advance_char();
        }

        let mut saw_digit = false;
        while matches!(self.peek_char(), Some('0'..='9')) {
            saw_digit = true;
            self.advance_char();
        }

        let mut is_float = false;
        if self.consume_char('.') {
            is_float = true;
            while matches!(self.peek_char(), Some('0'..='9')) {
                saw_digit = true;
                self.advance_char();
            }
        }

        if !saw_digit {
            return Err(PythonParseError::Unsupported);
        }

        if matches!(self.peek_char(), Some('e' | 'E')) {
            is_float = true;
            self.advance_char();
            if matches!(self.peek_char(), Some('+' | '-')) {
                self.advance_char();
            }
            let exp_start = self.pos;
            while matches!(self.peek_char(), Some('0'..='9')) {
                self.advance_char();
            }
            if self.pos == exp_start {
                return Err(PythonParseError::Unsupported);
            }
        }

        let raw = &self.text[start..self.pos];
        if is_float {
            let value = raw
                .parse::<f64>()
                .map_err(|_| PythonParseError::Unsupported)?;
            if !value.is_finite() {
                return Err(PythonParseError::Unsupported);
            }
            Ok(PythonLiteral::Scalar(Scalar::Float(value)))
        } else {
            let value = raw
                .parse::<i64>()
                .map_err(|_| PythonParseError::Unsupported)?;
            Ok(PythonLiteral::Scalar(Scalar::Int(value)))
        }
    }

    fn parse_string(&mut self) -> std::result::Result<String, PythonParseError> {
        let quote = self.advance_char().ok_or(PythonParseError::Incomplete)?;
        let mut out = String::new();
        loop {
            let ch = self.advance_char().ok_or(PythonParseError::Incomplete)?;
            if ch == quote {
                return Ok(out);
            }
            if ch == '\\' {
                let escaped = self.advance_char().ok_or(PythonParseError::Incomplete)?;
                match escaped {
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    '\\' => out.push('\\'),
                    '\'' => out.push('\''),
                    '"' => out.push('"'),
                    other => out.push(other),
                }
            } else {
                out.push(ch);
            }
        }
    }

    fn parse_identifier(&mut self) -> std::result::Result<String, PythonParseError> {
        self.skip_ws_and_comments();
        let start = self.pos;
        let end = parse_ident_end(self.text, start).ok_or_else(|| {
            if self.is_eof() {
                PythonParseError::Incomplete
            } else {
                PythonParseError::Unsupported
            }
        })?;
        self.pos = end;
        Ok(self.text[start..end].to_owned())
    }

    fn skip_ws_and_comments(&mut self) {
        loop {
            while let Some(ch) = self.peek_char() {
                if !ch.is_whitespace() {
                    break;
                }
                self.advance_char();
            }
            if !self.consume_char('#') {
                break;
            }
            while let Some(ch) = self.peek_char() {
                self.advance_char();
                if ch == '\n' {
                    break;
                }
            }
        }
    }

    fn expect_char(&mut self, expected: char) -> std::result::Result<(), PythonParseError> {
        if self.consume_char(expected) {
            Ok(())
        } else if self.is_eof() {
            Err(PythonParseError::Incomplete)
        } else {
            Err(PythonParseError::Unsupported)
        }
    }

    fn consume_char(&mut self, expected: char) -> bool {
        if self.peek_char() == Some(expected) {
            self.advance_char();
            true
        } else {
            false
        }
    }

    fn peek_char(&self) -> Option<char> {
        self.text[self.pos..].chars().next()
    }

    fn advance_char(&mut self) -> Option<char> {
        let ch = self.peek_char()?;
        self.pos += ch.len_utf8();
        Some(ch)
    }

    fn is_eof(&self) -> bool {
        self.pos >= self.text.len()
    }
}

fn literal_to_python_key(value: &PythonLiteral) -> std::result::Result<String, PythonParseError> {
    match value {
        PythonLiteral::Scalar(Scalar::String(s)) => Ok(s.clone()),
        PythonLiteral::Scalar(Scalar::Bool(b)) => Ok(b.to_string()),
        PythonLiteral::Scalar(Scalar::Int(i)) => Ok(i.to_string()),
        PythonLiteral::Scalar(Scalar::Float(f)) => Ok(format_float(*f)),
        PythonLiteral::Scalar(Scalar::Null) => Ok("null".to_owned()),
        PythonLiteral::Sequence(_) | PythonLiteral::Mapping(_) => {
            Err(PythonParseError::Unsupported)
        }
    }
}

fn flatten_python(value: &PythonLiteral, prefix: &str, out: &mut ScalarMap) -> Result<()> {
    match value {
        PythonLiteral::Scalar(s) => insert_scalar(out, prefix.to_owned(), s.clone()),
        PythonLiteral::Sequence(items) => {
            for (i, item) in items.iter().enumerate() {
                let key = push_key(prefix, &i.to_string());
                flatten_python(item, &key, out)?;
            }
            Ok(())
        }
        PythonLiteral::Mapping(map) => {
            for (key, value) in map {
                let key = push_key(prefix, key);
                flatten_python(value, &key, out)?;
            }
            Ok(())
        }
    }
}

fn flatten_yaml(value: &serde_yaml::Value, prefix: &str, out: &mut ScalarMap) -> Result<()> {
    use serde_yaml::Value;
    match value {
        Value::Null => insert_scalar(out, prefix.to_owned(), Scalar::Null),
        Value::Bool(b) => insert_scalar(out, prefix.to_owned(), Scalar::Bool(*b)),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                insert_scalar(out, prefix.to_owned(), Scalar::Int(i))
            } else if let Some(f) = n.as_f64() {
                insert_scalar(out, prefix.to_owned(), Scalar::Float(f))
            } else {
                // u64 that doesn't fit i64 — promote to float with a
                // round-trip that loses precision above 2^53. That's
                // acceptable for params/metrics diff; callers who
                // care about exact u64 values should store them as
                // strings.
                Err(WorkflowError::ParamsInvalid {
                    key: format!("numeric value at '{prefix}' out of i64/f64 range"),
                    origin: "params".into(),
                })
            }
        }
        Value::String(s) => insert_scalar(out, prefix.to_owned(), Scalar::String(s.clone())),
        Value::Sequence(items) => {
            if prefix.is_empty() {
                return Err(WorkflowError::ParamsInvalid {
                    key: "root value must be a mapping, not a sequence".into(),
                    origin: "params".into(),
                });
            }
            for (i, item) in items.iter().enumerate() {
                let key = push_key(prefix, &i.to_string());
                flatten_yaml(item, &key, out)?;
            }
            Ok(())
        }
        Value::Mapping(m) => {
            for (k, v) in m {
                let k_str = match k {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    Value::Bool(b) => b.to_string(),
                    _ => {
                        return Err(WorkflowError::ParamsInvalid {
                            key: format!("non-string key under '{prefix}'"),
                            origin: "params".into(),
                        });
                    }
                };
                let key = push_key(prefix, &k_str);
                flatten_yaml(v, &key, out)?;
            }
            Ok(())
        }
        Value::Tagged(t) => flatten_yaml(&t.value, prefix, out),
    }
}

fn flatten_json(value: &serde_json::Value, prefix: &str, out: &mut ScalarMap) -> Result<()> {
    use serde_json::Value;
    match value {
        Value::Null => insert_scalar(out, prefix.to_owned(), Scalar::Null),
        Value::Bool(b) => insert_scalar(out, prefix.to_owned(), Scalar::Bool(*b)),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                insert_scalar(out, prefix.to_owned(), Scalar::Int(i))
            } else if let Some(f) = n.as_f64() {
                insert_scalar(out, prefix.to_owned(), Scalar::Float(f))
            } else {
                Err(WorkflowError::ParamsInvalid {
                    key: format!("numeric value at '{prefix}' out of i64/f64 range"),
                    origin: "params".into(),
                })
            }
        }
        Value::String(s) => insert_scalar(out, prefix.to_owned(), Scalar::String(s.clone())),
        Value::Array(items) => {
            if prefix.is_empty() {
                return Err(WorkflowError::ParamsInvalid {
                    key: "root value must be an object, not an array".into(),
                    origin: "params".into(),
                });
            }
            for (i, item) in items.iter().enumerate() {
                let key = push_key(prefix, &i.to_string());
                flatten_json(item, &key, out)?;
            }
            Ok(())
        }
        Value::Object(m) => {
            for (k, v) in m {
                let key = push_key(prefix, k);
                flatten_json(v, &key, out)?;
            }
            Ok(())
        }
    }
}

fn flatten_toml(value: &toml::Value, prefix: &str, out: &mut ScalarMap) -> Result<()> {
    use toml::Value;
    match value {
        Value::Boolean(b) => insert_scalar(out, prefix.to_owned(), Scalar::Bool(*b)),
        Value::Integer(i) => insert_scalar(out, prefix.to_owned(), Scalar::Int(*i)),
        Value::Float(f) => insert_scalar(out, prefix.to_owned(), Scalar::Float(*f)),
        Value::String(s) => insert_scalar(out, prefix.to_owned(), Scalar::String(s.clone())),
        Value::Datetime(dt) => {
            // Represent datetimes as their canonical RFC-3339
            // string — round-trip is exact and the diff UI treats
            // them as opaque tokens.
            insert_scalar(out, prefix.to_owned(), Scalar::String(dt.to_string()))
        }
        Value::Array(items) => {
            if prefix.is_empty() {
                return Err(WorkflowError::ParamsInvalid {
                    key: "root value must be a table, not an array".into(),
                    origin: "params".into(),
                });
            }
            for (i, item) in items.iter().enumerate() {
                let key = push_key(prefix, &i.to_string());
                flatten_toml(item, &key, out)?;
            }
            Ok(())
        }
        Value::Table(t) => {
            for (k, v) in t {
                let key = push_key(prefix, k);
                flatten_toml(v, &key, out)?;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod tests {
    use super::*;

    #[test]
    fn yaml_parser_flattens_nested_maps_and_arrays() {
        let map = parse_yaml("model:\n  lr: 0.01\nwidths: [64, 128]\n").unwrap();

        assert_eq!(map.get("model.lr"), Some(&Scalar::Float(0.01)));
        assert_eq!(map.get("widths.0"), Some(&Scalar::Int(64)));
        assert_eq!(map.get("widths.1"), Some(&Scalar::Int(128)));
    }

    #[test]
    fn dispatch_uses_extension_contract() {
        assert!(parse(b"a: 1", Path::new("params.yaml")).is_ok());
        assert!(parse(br#"{"a": 1}"#, Path::new("params.json")).is_ok());
        assert!(parse(b"a = 1", Path::new("params.toml")).is_ok());
        assert!(parse(b"a = 1", Path::new("params.py")).is_ok());

        let err = parse(b"", Path::new("params.txt")).unwrap_err();
        assert!(matches!(err, WorkflowError::ParamsInvalid { .. }));
    }

    #[test]
    fn python_parser_reads_literal_assignments_without_executing_code() {
        let map = parse_python(
            r#"
import os

lr = 0.01
enabled = True
name = 'resnet'
widths = [64, 128]
optim = dict(kind="adam", weight_decay=0.001)
dynamic = os.getenv("DYNAMIC")

class TrainConfig:
    EPOCHS = 70

    def __init__(self):
        self.layers = 9
        self.sum = 1 + 2
"#,
        )
        .unwrap();

        assert_eq!(map.get("lr"), Some(&Scalar::Float(0.01)));
        assert_eq!(map.get("enabled"), Some(&Scalar::Bool(true)));
        assert_eq!(map.get("name"), Some(&Scalar::String("resnet".into())));
        assert_eq!(map.get("widths.0"), Some(&Scalar::Int(64)));
        assert_eq!(map.get("optim.kind"), Some(&Scalar::String("adam".into())));
        assert_eq!(map.get("TrainConfig.EPOCHS"), Some(&Scalar::Int(70)));
        assert_eq!(map.get("TrainConfig.layers"), Some(&Scalar::Int(9)));
        assert!(!map.contains_key("dynamic"));
        assert!(!map.contains_key("TrainConfig.sum"));
    }

    #[test]
    fn parser_rejects_non_finite_floats() {
        let err = parse_yaml("x: .nan\n").unwrap_err();

        assert!(matches!(err, WorkflowError::ParamsInvalid { .. }));
    }

    proptest::proptest! {
        #[test]
        fn json_round_trip_preserves_flat_scalar_maps(map in arb_scalar_map()) {
            let text = to_json(&map);
            let parsed = parse_json(&text).expect("parse json");
            assert_eq!(parsed, map);
        }
    }

    fn arb_scalar_map() -> impl proptest::strategy::Strategy<Value = ScalarMap> {
        use proptest::prelude::*;

        let key = "[a-z][a-z0-9_]{0,7}";
        let scalar = prop_oneof![
            any::<bool>().prop_map(Scalar::Bool),
            any::<i32>().prop_map(|i| Scalar::Int(i64::from(i))),
            (-10_000i32..10_000i32).prop_map(|n| Scalar::Float(f64::from(n) / 10_000.0)),
            "[a-z0-9 _-]{0,16}".prop_map(Scalar::String),
            Just(Scalar::Null),
        ];

        proptest::collection::btree_map(key, scalar, 0..8)
    }

    fn to_json(map: &ScalarMap) -> String {
        let mut root = serde_json::Map::new();
        for (key, value) in map {
            root.insert(key.clone(), scalar_to_json(value));
        }
        serde_json::to_string(&serde_json::Value::Object(root)).expect("json serialize")
    }

    fn scalar_to_json(scalar: &Scalar) -> serde_json::Value {
        match scalar {
            Scalar::Null => serde_json::Value::Null,
            Scalar::Bool(value) => serde_json::Value::Bool(*value),
            Scalar::Int(value) => serde_json::Value::Number((*value).into()),
            Scalar::Float(value) => serde_json::Number::from_f64(*value)
                .map(serde_json::Value::Number)
                .expect("finite float"),
            Scalar::String(value) => serde_json::Value::String(value.clone()),
        }
    }
}
