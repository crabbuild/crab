//! GraphQL lexer and recursive-descent parser for executable documents and
//! the type-system subset used by the embedded GitHub schema.

use std::fmt;

/// Bounds parser recursion so hostile nesting cannot exhaust the stack.
const MAX_DEPTH: usize = 64;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Position {
    pub(crate) line: usize,
    pub(crate) column: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SyntaxError {
    pub(crate) message: String,
    pub(crate) position: Position,
}

impl fmt::Display for SyntaxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} (line {}, column {})",
            self.message, self.position.line, self.position.column
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Value {
    Variable(String),
    Int(i64),
    Float(f64),
    String(String),
    Boolean(bool),
    Null,
    Enum(String),
    List(Vec<Value>),
    Object(Vec<(String, Value)>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Type {
    Named(String),
    List(Box<Type>),
    NonNull(Box<Type>),
}

impl Type {
    pub(crate) fn named(&self) -> &str {
        match self {
            Self::Named(name) => name,
            Self::List(inner) | Self::NonNull(inner) => inner.named(),
        }
    }

    pub(crate) fn is_non_null(&self) -> bool {
        matches!(self, Self::NonNull(_))
    }
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Named(name) => f.write_str(name),
            Self::List(inner) => write!(f, "[{inner}]"),
            Self::NonNull(inner) => write!(f, "{inner}!"),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Directive {
    pub(crate) name: String,
    pub(crate) arguments: Vec<(String, Value)>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Selection {
    Field(Field),
    FragmentSpread {
        name: String,
        directives: Vec<Directive>,
        position: Position,
    },
    InlineFragment {
        type_condition: Option<String>,
        directives: Vec<Directive>,
        selections: Vec<Selection>,
        position: Position,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Field {
    pub(crate) alias: Option<String>,
    pub(crate) name: String,
    pub(crate) arguments: Vec<(String, Value)>,
    pub(crate) directives: Vec<Directive>,
    pub(crate) selections: Vec<Selection>,
    pub(crate) position: Position,
}

impl Field {
    pub(crate) fn response_key(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.name)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OperationKind {
    Query,
    Mutation,
    Subscription,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct VariableDefinition {
    pub(crate) name: String,
    pub(crate) ty: Type,
    pub(crate) default: Option<Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Operation {
    pub(crate) kind: OperationKind,
    pub(crate) name: Option<String>,
    pub(crate) variables: Vec<VariableDefinition>,
    pub(crate) selections: Vec<Selection>,
    pub(crate) position: Position,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Fragment {
    pub(crate) name: String,
    pub(crate) type_condition: String,
    pub(crate) selections: Vec<Selection>,
    pub(crate) position: Position,
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct Document {
    pub(crate) operations: Vec<Operation>,
    pub(crate) fragments: Vec<Fragment>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TypeKind {
    Scalar,
    Object,
    Interface,
    Union,
    Enum,
    InputObject,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct InputValueDefinition {
    pub(crate) name: String,
    pub(crate) ty: Type,
    pub(crate) default: Option<Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FieldDefinition {
    pub(crate) name: String,
    pub(crate) arguments: Vec<InputValueDefinition>,
    pub(crate) ty: Type,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TypeDefinition {
    pub(crate) kind: TypeKind,
    pub(crate) name: String,
    pub(crate) interfaces: Vec<String>,
    pub(crate) fields: Vec<FieldDefinition>,
    pub(crate) members: Vec<String>,
    pub(crate) values: Vec<String>,
    pub(crate) inputs: Vec<InputValueDefinition>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Token<'a> {
    Punctuator(char),
    Spread,
    Name(&'a str),
    Int(i64),
    Float(f64),
    String,
    End,
}

struct Lexer<'a> {
    source: &'a str,
    offset: usize,
    line: usize,
    line_start: usize,
    // Decoded string literal contents for the most recent `Token::String`.
    string: String,
}

impl<'a> Lexer<'a> {
    fn new(source: &'a str) -> Self {
        let source = source.strip_prefix('\u{feff}').unwrap_or(source);
        Self {
            source,
            offset: 0,
            line: 1,
            line_start: 0,
            string: String::new(),
        }
    }

    fn position(&self) -> Position {
        Position {
            line: self.line,
            column: self.offset - self.line_start + 1,
        }
    }

    fn error(&self, message: impl Into<String>) -> SyntaxError {
        SyntaxError {
            message: message.into(),
            position: self.position(),
        }
    }

    fn peek_byte(&self) -> Option<u8> {
        self.source.as_bytes().get(self.offset).copied()
    }

    fn newline(&mut self) {
        self.line += 1;
        self.line_start = self.offset;
    }

    fn skip_ignored(&mut self) {
        while let Some(byte) = self.peek_byte() {
            match byte {
                b' ' | b'\t' | b',' => self.offset += 1,
                b'\n' => {
                    self.offset += 1;
                    self.newline();
                }
                b'\r' => {
                    self.offset += 1;
                    if self.peek_byte() == Some(b'\n') {
                        self.offset += 1;
                    }
                    self.newline();
                }
                b'#' => {
                    while let Some(byte) = self.peek_byte() {
                        if byte == b'\n' || byte == b'\r' {
                            break;
                        }
                        self.offset += 1;
                    }
                }
                _ => return,
            }
        }
    }

    fn next(&mut self) -> Result<(Token<'a>, Position), SyntaxError> {
        self.skip_ignored();
        let position = self.position();
        let Some(byte) = self.peek_byte() else {
            return Ok((Token::End, position));
        };
        let token = match byte {
            b'!' | b'$' | b'&' | b'(' | b')' | b':' | b'=' | b'@' | b'[' | b']' | b'{' | b'|'
            | b'}' => {
                self.offset += 1;
                Token::Punctuator(char::from(byte))
            }
            b'.' => {
                if self.source[self.offset..].starts_with("...") {
                    self.offset += 3;
                    Token::Spread
                } else {
                    return Err(self.error("Expected '...'"));
                }
            }
            b'"' => {
                self.string = if self.source[self.offset..].starts_with("\"\"\"") {
                    self.block_string()?
                } else {
                    self.quoted_string()?
                };
                Token::String
            }
            b'-' | b'0'..=b'9' => self.number()?,
            b'_' | b'a'..=b'z' | b'A'..=b'Z' => {
                let start = self.offset;
                while let Some(byte) = self.peek_byte() {
                    if byte == b'_' || byte.is_ascii_alphanumeric() {
                        self.offset += 1;
                    } else {
                        break;
                    }
                }
                Token::Name(&self.source[start..self.offset])
            }
            _ => {
                let character = self.source[self.offset..].chars().next().unwrap_or('?');
                return Err(self.error(format!("Unexpected character {character:?}")));
            }
        };
        Ok((token, position))
    }

    fn number(&mut self) -> Result<Token<'a>, SyntaxError> {
        let start = self.offset;
        if self.peek_byte() == Some(b'-') {
            self.offset += 1;
        }
        let digits = |lexer: &mut Self| {
            let begin = lexer.offset;
            while lexer.peek_byte().is_some_and(|byte| byte.is_ascii_digit()) {
                lexer.offset += 1;
            }
            lexer.offset > begin
        };
        if !digits(self) {
            return Err(self.error("Expected a digit"));
        }
        let mut float = false;
        if self.peek_byte() == Some(b'.') {
            float = true;
            self.offset += 1;
            if !digits(self) {
                return Err(self.error("Expected a digit after '.'"));
            }
        }
        if matches!(self.peek_byte(), Some(b'e' | b'E')) {
            float = true;
            self.offset += 1;
            if matches!(self.peek_byte(), Some(b'+' | b'-')) {
                self.offset += 1;
            }
            if !digits(self) {
                return Err(self.error("Expected an exponent digit"));
            }
        }
        if self
            .peek_byte()
            .is_some_and(|byte| byte == b'.' || byte == b'_' || byte.is_ascii_alphabetic())
        {
            return Err(self.error("Invalid number"));
        }
        let text = &self.source[start..self.offset];
        if float {
            text.parse()
                .map(Token::Float)
                .map_err(|_| self.error("Invalid float"))
        } else {
            text.parse()
                .map(Token::Int)
                .map_err(|_| self.error("Integer out of range"))
        }
    }

    fn quoted_string(&mut self) -> Result<String, SyntaxError> {
        self.offset += 1;
        let mut value = String::new();
        loop {
            let Some(character) = self.source[self.offset..].chars().next() else {
                return Err(self.error("Unterminated string"));
            };
            match character {
                '"' => {
                    self.offset += 1;
                    return Ok(value);
                }
                '\n' | '\r' => return Err(self.error("Unterminated string")),
                '\\' => {
                    self.offset += 1;
                    let Some(escape) = self.peek_byte() else {
                        return Err(self.error("Unterminated string"));
                    };
                    self.offset += 1;
                    match escape {
                        b'"' => value.push('"'),
                        b'\\' => value.push('\\'),
                        b'/' => value.push('/'),
                        b'b' => value.push('\u{8}'),
                        b'f' => value.push('\u{c}'),
                        b'n' => value.push('\n'),
                        b'r' => value.push('\r'),
                        b't' => value.push('\t'),
                        b'u' => value.push(self.unicode_escape()?),
                        _ => return Err(self.error("Invalid escape sequence")),
                    }
                }
                _ => {
                    value.push(character);
                    self.offset += character.len_utf8();
                }
            }
        }
    }

    fn unicode_escape(&mut self) -> Result<char, SyntaxError> {
        let hex = |lexer: &mut Self, length: usize| {
            let text = lexer
                .source
                .get(lexer.offset..lexer.offset + length)
                .filter(|text| text.bytes().all(|byte| byte.is_ascii_hexdigit()))
                .ok_or_else(|| lexer.error("Invalid unicode escape"))?;
            lexer.offset += length;
            u32::from_str_radix(text, 16).map_err(|_| lexer.error("Invalid unicode escape"))
        };
        if self.peek_byte() == Some(b'{') {
            self.offset += 1;
            let end = self.source[self.offset..]
                .find('}')
                .ok_or_else(|| self.error("Invalid unicode escape"))?;
            let code = hex(self, end)?;
            self.offset += 1;
            return char::from_u32(code).ok_or_else(|| self.error("Invalid unicode escape"));
        }
        let high = hex(self, 4)?;
        if (0xd800..0xdc00).contains(&high) {
            if !self.source[self.offset..].starts_with("\\u") {
                return Err(self.error("Invalid unicode surrogate"));
            }
            self.offset += 2;
            let low = hex(self, 4)?;
            if !(0xdc00..0xe000).contains(&low) {
                return Err(self.error("Invalid unicode surrogate"));
            }
            let code = 0x10000 + ((high - 0xd800) << 10) + (low - 0xdc00);
            return char::from_u32(code).ok_or_else(|| self.error("Invalid unicode escape"));
        }
        char::from_u32(high).ok_or_else(|| self.error("Invalid unicode escape"))
    }

    fn block_string(&mut self) -> Result<String, SyntaxError> {
        self.offset += 3;
        let mut raw = String::new();
        loop {
            if self.source[self.offset..].starts_with("\"\"\"") {
                self.offset += 3;
                return Ok(block_string_value(&raw));
            }
            if self.source[self.offset..].starts_with("\\\"\"\"") {
                raw.push_str("\"\"\"");
                self.offset += 4;
                continue;
            }
            let Some(character) = self.source[self.offset..].chars().next() else {
                return Err(self.error("Unterminated block string"));
            };
            self.offset += character.len_utf8();
            match character {
                '\n' => {
                    raw.push('\n');
                    self.newline();
                }
                '\r' => {
                    if self.peek_byte() == Some(b'\n') {
                        self.offset += 1;
                    }
                    raw.push('\n');
                    self.newline();
                }
                _ => raw.push(character),
            }
        }
    }
}

// The specification's BlockStringValue: strip common indentation and blank
// leading/trailing lines so descriptions read like their source.
fn block_string_value(raw: &str) -> String {
    let lines: Vec<&str> = raw.split('\n').collect();
    let indent = lines
        .iter()
        .skip(1)
        .filter_map(|line| {
            let trimmed = line.trim_start_matches([' ', '\t']);
            (!trimmed.is_empty()).then(|| line.len() - trimmed.len())
        })
        .min()
        .unwrap_or(0);
    let mut lines: Vec<&str> = lines
        .iter()
        .enumerate()
        .map(|(index, line)| {
            if index == 0 {
                line
            } else {
                line.get(indent..).unwrap_or("")
            }
        })
        .collect();
    while lines.first().is_some_and(|line| line.trim().is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

struct Parser<'a> {
    lexer: Lexer<'a>,
    token: Token<'a>,
    position: Position,
    depth: usize,
}

impl<'a> Parser<'a> {
    fn new(source: &'a str) -> Result<Self, SyntaxError> {
        let mut lexer = Lexer::new(source);
        let (token, position) = lexer.next()?;
        Ok(Self {
            lexer,
            token,
            position,
            depth: 0,
        })
    }

    fn advance(&mut self) -> Result<Token<'a>, SyntaxError> {
        let (token, position) = self.lexer.next()?;
        self.position = position;
        Ok(std::mem::replace(&mut self.token, token))
    }

    fn error(&self, message: impl Into<String>) -> SyntaxError {
        SyntaxError {
            message: message.into(),
            position: self.position,
        }
    }

    fn unexpected(&self) -> SyntaxError {
        let found = match self.token {
            Token::Punctuator(character) => format!("'{character}'"),
            Token::Spread => "'...'".into(),
            Token::Name(name) => format!("'{name}'"),
            Token::Int(value) => value.to_string(),
            Token::Float(value) => value.to_string(),
            Token::String => "string".into(),
            Token::End => "end of document".into(),
        };
        self.error(format!("Unexpected {found}"))
    }

    fn at(&self, punctuator: char) -> bool {
        self.token == Token::Punctuator(punctuator)
    }

    fn at_keyword(&self, keyword: &str) -> bool {
        self.token == Token::Name(keyword)
    }

    fn expect(&mut self, punctuator: char) -> Result<(), SyntaxError> {
        if self.at(punctuator) {
            self.advance()?;
            Ok(())
        } else {
            Err(self
                .error(format!("Expected '{punctuator}'"))
                .or_unexpected(self))
        }
    }

    fn take(&mut self, punctuator: char) -> Result<bool, SyntaxError> {
        if self.at(punctuator) {
            self.advance()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn name(&mut self) -> Result<String, SyntaxError> {
        match self.token {
            Token::Name(name) => {
                let name = name.to_owned();
                self.advance()?;
                Ok(name)
            }
            _ => Err(self.error("Expected a name").or_unexpected(self)),
        }
    }

    fn enter(&mut self) -> Result<(), SyntaxError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.error("Document is nested too deeply"));
        }
        Ok(())
    }

    fn leave(&mut self) {
        self.depth -= 1;
    }

    fn executable(mut self) -> Result<Document, SyntaxError> {
        let mut document = Document::default();
        while self.token != Token::End {
            let position = self.position;
            if self.at('{') {
                document.operations.push(Operation {
                    kind: OperationKind::Query,
                    name: None,
                    variables: Vec::new(),
                    selections: self.selection_set()?,
                    position,
                });
                continue;
            }
            let Token::Name(keyword) = self.token else {
                return Err(self.unexpected());
            };
            match keyword {
                "query" | "mutation" | "subscription" => {
                    let kind = match keyword {
                        "query" => OperationKind::Query,
                        "mutation" => OperationKind::Mutation,
                        _ => OperationKind::Subscription,
                    };
                    self.advance()?;
                    let name = match self.token {
                        Token::Name(_) => Some(self.name()?),
                        _ => None,
                    };
                    let variables = self.variable_definitions()?;
                    self.directives()?;
                    document.operations.push(Operation {
                        kind,
                        name,
                        variables,
                        selections: self.selection_set()?,
                        position,
                    });
                }
                "fragment" => {
                    self.advance()?;
                    let name = self.name()?;
                    if name == "on" {
                        return Err(self.error("Fragment name cannot be 'on'"));
                    }
                    if !self.at_keyword("on") {
                        return Err(self.error("Expected 'on'"));
                    }
                    self.advance()?;
                    let type_condition = self.name()?;
                    self.directives()?;
                    document.fragments.push(Fragment {
                        name,
                        type_condition,
                        selections: self.selection_set()?,
                        position,
                    });
                }
                _ => return Err(self.unexpected()),
            }
        }
        if document.operations.is_empty() {
            return Err(self.error("Document contains no operation"));
        }
        Ok(document)
    }

    fn variable_definitions(&mut self) -> Result<Vec<VariableDefinition>, SyntaxError> {
        let mut variables = Vec::new();
        if !self.take('(')? {
            return Ok(variables);
        }
        while !self.take(')')? {
            self.expect('$')?;
            let name = self.name()?;
            self.expect(':')?;
            let ty = self.type_reference()?;
            let default = if self.take('=')? {
                Some(self.value(true)?)
            } else {
                None
            };
            self.directives()?;
            variables.push(VariableDefinition { name, ty, default });
        }
        if variables.is_empty() {
            return Err(self.error("Expected a variable definition"));
        }
        Ok(variables)
    }

    fn type_reference(&mut self) -> Result<Type, SyntaxError> {
        self.enter()?;
        let ty = if self.take('[')? {
            let inner = self.type_reference()?;
            self.expect(']')?;
            Type::List(Box::new(inner))
        } else {
            Type::Named(self.name()?)
        };
        self.leave();
        if self.take('!')? {
            Ok(Type::NonNull(Box::new(ty)))
        } else {
            Ok(ty)
        }
    }

    fn directives(&mut self) -> Result<Vec<Directive>, SyntaxError> {
        let mut directives = Vec::new();
        while self.take('@')? {
            let name = self.name()?;
            let arguments = self.arguments(false)?;
            directives.push(Directive { name, arguments });
        }
        Ok(directives)
    }

    fn arguments(&mut self, constant: bool) -> Result<Vec<(String, Value)>, SyntaxError> {
        let mut arguments = Vec::new();
        if !self.take('(')? {
            return Ok(arguments);
        }
        while !self.take(')')? {
            let name = self.name()?;
            self.expect(':')?;
            arguments.push((name, self.value(constant)?));
        }
        if arguments.is_empty() {
            return Err(self.error("Expected an argument"));
        }
        Ok(arguments)
    }

    fn selection_set(&mut self) -> Result<Vec<Selection>, SyntaxError> {
        self.enter()?;
        self.expect('{')?;
        let mut selections = Vec::new();
        while !self.take('}')? {
            selections.push(self.selection()?);
        }
        if selections.is_empty() {
            return Err(self.error("Selection set cannot be empty"));
        }
        self.leave();
        Ok(selections)
    }

    fn selection(&mut self) -> Result<Selection, SyntaxError> {
        let position = self.position;
        if self.token == Token::Spread {
            self.advance()?;
            if self.at_keyword("on") {
                self.advance()?;
                let type_condition = Some(self.name()?);
                let directives = self.directives()?;
                return Ok(Selection::InlineFragment {
                    type_condition,
                    directives,
                    selections: self.selection_set()?,
                    position,
                });
            }
            if let Token::Name(_) = self.token {
                let name = self.name()?;
                return Ok(Selection::FragmentSpread {
                    name,
                    directives: self.directives()?,
                    position,
                });
            }
            let directives = self.directives()?;
            return Ok(Selection::InlineFragment {
                type_condition: None,
                directives,
                selections: self.selection_set()?,
                position,
            });
        }
        let first = self.name()?;
        let (alias, name) = if self.take(':')? {
            (Some(first), self.name()?)
        } else {
            (None, first)
        };
        let arguments = self.arguments(false)?;
        let directives = self.directives()?;
        let selections = if self.at('{') {
            self.selection_set()?
        } else {
            Vec::new()
        };
        Ok(Selection::Field(Field {
            alias,
            name,
            arguments,
            directives,
            selections,
            position,
        }))
    }

    fn value(&mut self, constant: bool) -> Result<Value, SyntaxError> {
        self.enter()?;
        let value = match self.token {
            Token::Punctuator('$') if !constant => {
                self.advance()?;
                Value::Variable(self.name()?)
            }
            Token::Int(value) => {
                self.advance()?;
                Value::Int(value)
            }
            Token::Float(value) => {
                self.advance()?;
                Value::Float(value)
            }
            Token::String => {
                let value = std::mem::take(&mut self.lexer.string);
                self.advance()?;
                Value::String(value)
            }
            Token::Name("true") => {
                self.advance()?;
                Value::Boolean(true)
            }
            Token::Name("false") => {
                self.advance()?;
                Value::Boolean(false)
            }
            Token::Name("null") => {
                self.advance()?;
                Value::Null
            }
            Token::Name(name) => {
                let name = name.to_owned();
                self.advance()?;
                Value::Enum(name)
            }
            Token::Punctuator('[') => {
                self.advance()?;
                let mut items = Vec::new();
                while !self.take(']')? {
                    items.push(self.value(constant)?);
                }
                Value::List(items)
            }
            Token::Punctuator('{') => {
                self.advance()?;
                let mut fields = Vec::new();
                while !self.take('}')? {
                    let name = self.name()?;
                    self.expect(':')?;
                    fields.push((name, self.value(constant)?));
                }
                Value::Object(fields)
            }
            _ => return Err(self.unexpected()),
        };
        self.leave();
        Ok(value)
    }

    fn schema(mut self) -> Result<Vec<TypeDefinition>, SyntaxError> {
        let mut types = Vec::new();
        while self.token != Token::End {
            self.description()?;
            let Token::Name(keyword) = self.token else {
                return Err(self.unexpected());
            };
            self.advance()?;
            let name = self.name()?;
            let mut definition = TypeDefinition {
                kind: TypeKind::Scalar,
                name,
                interfaces: Vec::new(),
                fields: Vec::new(),
                members: Vec::new(),
                values: Vec::new(),
                inputs: Vec::new(),
            };
            match keyword {
                "scalar" => {
                    self.directives()?;
                }
                "type" | "interface" => {
                    definition.kind = if keyword == "type" {
                        TypeKind::Object
                    } else {
                        TypeKind::Interface
                    };
                    if self.at_keyword("implements") {
                        self.advance()?;
                        self.take('&')?;
                        definition.interfaces.push(self.name()?);
                        while self.take('&')? {
                            definition.interfaces.push(self.name()?);
                        }
                    }
                    self.directives()?;
                    self.expect('{')?;
                    while !self.take('}')? {
                        definition.fields.push(self.field_definition()?);
                    }
                }
                "union" => {
                    definition.kind = TypeKind::Union;
                    self.directives()?;
                    self.expect('=')?;
                    self.take('|')?;
                    definition.members.push(self.name()?);
                    while self.take('|')? {
                        definition.members.push(self.name()?);
                    }
                }
                "enum" => {
                    definition.kind = TypeKind::Enum;
                    self.directives()?;
                    self.expect('{')?;
                    while !self.take('}')? {
                        self.description()?;
                        definition.values.push(self.name()?);
                        self.directives()?;
                    }
                }
                "input" => {
                    definition.kind = TypeKind::InputObject;
                    self.directives()?;
                    self.expect('{')?;
                    while !self.take('}')? {
                        definition.inputs.push(self.input_value_definition()?);
                    }
                }
                _ => return Err(self.error(format!("Unsupported definition '{keyword}'"))),
            }
            types.push(definition);
        }
        Ok(types)
    }

    fn description(&mut self) -> Result<(), SyntaxError> {
        if self.token == Token::String {
            self.advance()?;
        }
        Ok(())
    }

    fn field_definition(&mut self) -> Result<FieldDefinition, SyntaxError> {
        self.description()?;
        let name = self.name()?;
        let mut arguments = Vec::new();
        if self.take('(')? {
            while !self.take(')')? {
                arguments.push(self.input_value_definition()?);
            }
        }
        self.expect(':')?;
        let ty = self.type_reference()?;
        self.directives()?;
        Ok(FieldDefinition {
            name,
            arguments,
            ty,
        })
    }

    fn input_value_definition(&mut self) -> Result<InputValueDefinition, SyntaxError> {
        self.description()?;
        let name = self.name()?;
        self.expect(':')?;
        let ty = self.type_reference()?;
        let default = if self.take('=')? {
            Some(self.value(true)?)
        } else {
            None
        };
        self.directives()?;
        Ok(InputValueDefinition { name, ty, default })
    }
}

trait OrUnexpected {
    fn or_unexpected(self, parser: &Parser<'_>) -> SyntaxError;
}

impl OrUnexpected for SyntaxError {
    fn or_unexpected(self, parser: &Parser<'_>) -> SyntaxError {
        let unexpected = parser.unexpected();
        SyntaxError {
            message: format!("{}, found {}", self.message, &unexpected.message[11..]),
            position: unexpected.position,
        }
    }
}

/// Parses a client request document.
pub(crate) fn parse_document(source: &str) -> Result<Document, SyntaxError> {
    Parser::new(source)?.executable()
}

/// Parses the type-system definitions of the embedded schema.
pub(crate) fn parse_schema(source: &str) -> Result<Vec<TypeDefinition>, SyntaxError> {
    Parser::new(source)?.schema()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_operations_fragments_variables_and_aliases_from_gh_queries() {
        let document = parse_document(
            r#"
            fragment repo on Repository { id name owner { login } }
            query RepositoryNetwork($limit: Int = 30, $states: [IssueState!]!) {
              viewer { login }
              repo_000: repository(owner: "cli", name: "cli") { ...repo parent { ...repo } }
              search(type: ISSUE, last: $limit, query: "is:open") @include(if: true) {
                nodes { __typename ...on Issue { number } }
              }
            }
            "#,
        )
        .unwrap();
        let operation = &document.operations[0];
        assert_eq!(
            (
                operation.kind,
                operation.name.as_deref(),
                operation.variables.len(),
                operation.variables[1].ty.to_string(),
                document.fragments[0].type_condition.as_str(),
            ),
            (
                OperationKind::Query,
                Some("RepositoryNetwork"),
                2,
                "[IssueState!]!".to_owned(),
                "Repository",
            )
        );
        let Selection::Field(repository) = &operation.selections[1] else {
            panic!("expected field");
        };
        assert_eq!(
            (
                repository.response_key(),
                repository.name.as_str(),
                &repository.arguments[0]
            ),
            (
                "repo_000",
                "repository",
                &("owner".to_owned(), Value::String("cli".into()))
            )
        );
    }

    #[test]
    fn decodes_string_escapes_block_strings_and_numbers() {
        let document = parse_document(
            r#"{ a(s: "tab\t\"q\" é \u{1F600} 😀", b: """
                  first
                    second
                """, i: -12, f: 1.5e2) }"#,
        )
        .unwrap();
        let Selection::Field(field) = &document.operations[0].selections[0] else {
            panic!("expected field");
        };
        assert_eq!(
            field.arguments,
            vec![
                ("s".into(), Value::String("tab\t\"q\" é 😀 😀".into())),
                ("b".into(), Value::String("first\n  second".into())),
                ("i".into(), Value::Int(-12)),
                ("f".into(), Value::Float(150.0)),
            ]
        );
    }

    #[test]
    fn rejects_malformed_and_hostile_documents() {
        for source in [
            "",
            "{ }",
            "query { a(",
            "{ a(b: \"unterminated) }",
            "{ a(b: 01x) }",
            "fragment on on T { a }",
            "{ a(b: 99999999999999999999) }",
        ] {
            assert!(parse_document(source).is_err(), "{source:?}");
        }
        let deep = format!("{}{}", "{ a ".repeat(100), "}".repeat(100));
        assert_eq!(
            parse_document(&deep).unwrap_err().message,
            "Document is nested too deeply"
        );
    }

    #[test]
    fn parses_schema_definitions() {
        let types = parse_schema(
            r#"
            "A thing" scalar DateTime
            interface Node { id: ID! }
            type Repository implements Node & RepositoryInfo {
              id: ID!
              issues(first: Int, states: [IssueState!] = [OPEN]): IssueConnection!
            }
            union IssueOrPullRequest = | Issue | PullRequest
            enum IssueState { OPEN CLOSED }
            input IssueOrder { field: IssueOrderField! direction: OrderDirection! }
            "#,
        )
        .unwrap();
        assert_eq!(
            types
                .iter()
                .map(|ty| (ty.kind, ty.name.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (TypeKind::Scalar, "DateTime"),
                (TypeKind::Interface, "Node"),
                (TypeKind::Object, "Repository"),
                (TypeKind::Union, "IssueOrPullRequest"),
                (TypeKind::Enum, "IssueState"),
                (TypeKind::InputObject, "IssueOrder"),
            ]
        );
        assert_eq!(types[2].interfaces, ["Node", "RepositoryInfo"]);
        assert_eq!(
            types[2].fields[1].arguments[1].ty.to_string(),
            "[IssueState!]"
        );
        assert_eq!(types[3].members, ["Issue", "PullRequest"]);
    }
}
