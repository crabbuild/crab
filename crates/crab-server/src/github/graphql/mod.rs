//! A bounded GraphQL executor for the GitHub-compatible API.
//!
//! Documents are validated against the embedded schema before any resolver
//! runs, so unknown fields fail with GitHub's messages instead of returning
//! fabricated values. Execution is sequential, follows the specification's
//! null propagation, and charges every resolved object against a budget.

mod schema;
mod syntax;

#[cfg(test)]
mod tests;

pub(crate) use schema::Schema;

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};

use schema::{SchemaObject, TypeObject};
use syntax::{
    Field, Fragment, Operation, OperationKind, Position, Selection, Type, Value as Literal,
};

pub(crate) const MAX_DOCUMENT_BYTES: usize = 64 * 1024;
const MAX_RESOLVED_OBJECTS: usize = 25_000;
const MAX_ERRORS: usize = 50;

/// A resolver failure, reported with GitHub's error `type` when it has one.
#[derive(Debug)]
pub(crate) struct FieldError {
    message: String,
    kind: Option<&'static str>,
}

impl FieldError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: None,
        }
    }

    pub(crate) fn typed(kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: Some(kind),
        }
    }

    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self::typed("NOT_FOUND", message)
    }

    pub(crate) fn forbidden(message: impl Into<String>) -> Self {
        Self::typed("FORBIDDEN", message)
    }

    pub(crate) fn unprocessable(message: impl Into<String>) -> Self {
        Self::typed("UNPROCESSABLE", message)
    }

    pub(crate) fn kind(&self) -> Option<&'static str> {
        self.kind
    }

    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

/// A resolved field value before it is completed against the schema type.
pub(crate) enum Resolved<C> {
    Leaf(Value),
    Object(Box<dyn Object<C>>),
    List(Vec<Resolved<C>>),
}

impl<C> Resolved<C> {
    pub(crate) fn null() -> Self {
        Self::Leaf(Value::Null)
    }

    pub(crate) fn leaf(value: impl Into<Value>) -> Self {
        Self::Leaf(value.into())
    }

    pub(crate) fn object(object: impl Object<C> + 'static) -> Self
    where
        C: Sync,
    {
        Self::Object(Box::new(object))
    }

    pub(crate) fn optional(object: Option<impl Object<C> + 'static>) -> Self
    where
        C: Sync,
    {
        object.map_or_else(Self::null, Self::object)
    }
}

/// A GraphQL object. Validation guarantees `resolve` only receives fields
/// that the schema defines for the object's concrete type.
#[allow(
    clippy::double_must_use,
    reason = "async_trait marks boxed futures must-use; Result is also must-use"
)]
#[async_trait]
pub(crate) trait Object<C: Sync>: Send + Sync {
    fn typename(&self) -> &str;

    async fn resolve(
        &self,
        context: &C,
        field: &FieldRequest<'_>,
    ) -> Result<Resolved<C>, FieldError>;
}

/// The coerced arguments and merged sub-selection of one response field.
pub(crate) struct FieldRequest<'a> {
    pub(crate) name: &'a str,
    pub(crate) arguments: Map<String, Value>,
    fields: Vec<&'a Field>,
    fragments: &'a HashMap<&'a str, &'a Fragment>,
}

impl FieldRequest<'_> {
    /// A present, non-null argument.
    pub(crate) fn argument(&self, name: &str) -> Option<&Value> {
        self.arguments.get(name).filter(|value| !value.is_null())
    }

    pub(crate) fn string(&self, name: &str) -> Option<&str> {
        self.argument(name).and_then(Value::as_str)
    }

    pub(crate) fn int(&self, name: &str) -> Option<i64> {
        self.argument(name).and_then(Value::as_i64)
    }

    /// Whether the client selected `path` beneath this field, through any
    /// fragment. Type conditions are ignored, so this may over-approximate.
    pub(crate) fn selects(&self, path: &[&str]) -> bool {
        fn search(
            selections: &[Selection],
            path: &[&str],
            fragments: &HashMap<&str, &Fragment>,
            visited: &mut HashSet<String>,
        ) -> bool {
            let Some((first, rest)) = path.split_first() else {
                return true;
            };
            selections.iter().any(|selection| match selection {
                Selection::Field(field) => {
                    field.name == *first && search(&field.selections, rest, fragments, visited)
                }
                Selection::InlineFragment { selections, .. } => {
                    search(selections, path, fragments, visited)
                }
                Selection::FragmentSpread { name, .. } => {
                    visited.insert(name.clone())
                        && fragments.get(name.as_str()).is_some_and(|fragment| {
                            search(&fragment.selections, path, fragments, visited)
                        })
                }
            })
        }
        self.fields
            .iter()
            .any(|field| search(&field.selections, path, self.fragments, &mut HashSet::new()))
    }
}

pub(crate) struct Request {
    pub(crate) query: String,
    pub(crate) operation_name: Option<String>,
    pub(crate) variables: Map<String, Value>,
}

/// Executes `request` and returns the complete GraphQL response document.
pub(crate) async fn execute<C: Sync>(
    schema: &'static Schema,
    context: &C,
    query: &dyn Object<C>,
    mutation: &dyn Object<C>,
    request: &Request,
) -> Value {
    if request.query.len() > MAX_DOCUMENT_BYTES {
        return json!({"errors":[{"message":"Query document is too large"}]});
    }
    let document = match syntax::parse_document(&request.query) {
        Ok(document) => document,
        Err(error) => {
            return json!({"errors":[{
                "message": format!("Parse error: {}", error.message),
                "locations": [location(error.position)],
            }]});
        }
    };
    let operation = match request.operation_name.as_deref() {
        Some(name) => document
            .operations
            .iter()
            .find(|operation| operation.name.as_deref() == Some(name)),
        None if document.operations.len() == 1 => document.operations.first(),
        None => {
            return json!({"errors":[{"message":"An operation name is required"}]});
        }
    };
    let Some(operation) = operation else {
        return json!({"errors":[{"message":"No operation named the requested operation"}]});
    };
    let (root, root_type) = match operation.kind {
        OperationKind::Query => (query, "Query"),
        OperationKind::Mutation if schema.mutation().is_some() => (mutation, "Mutation"),
        OperationKind::Mutation => {
            return json!({"errors":[{"message":"Schema is not configured for mutations"}]});
        }
        OperationKind::Subscription => {
            return json!({"errors":[{"message":"Subscriptions are not supported"}]});
        }
    };
    let mut fragments = HashMap::new();
    for fragment in &document.fragments {
        if fragments.insert(fragment.name.as_str(), fragment).is_some() {
            return json!({"errors":[{
                "message": format!("Fragment name \"{}\" must be unique", fragment.name),
                "locations": [location(fragment.position)],
            }]});
        }
    }
    let mut validation = Validation {
        schema,
        operation,
        fragments: &fragments,
        errors: Vec::new(),
        validated: HashSet::new(),
        stack: Vec::new(),
    };
    validation.selection_set(root_type, &operation.selections);
    if !validation.errors.is_empty() {
        return json!({"errors": validation.errors});
    }
    let variables = match coerce_variables(schema, operation, &request.variables) {
        Ok(variables) => variables,
        Err(errors) => return json!({"errors": errors}),
    };
    let execution = Execution {
        schema,
        context,
        fragments: &fragments,
        variables,
        errors: Mutex::new(Vec::new()),
        resolved: AtomicUsize::new(0),
    };
    let data = execution
        .selection_set(root, &[operation.selections.as_slice()], &Path::Root)
        .await
        .map_or(Value::Null, Value::Object);
    let errors = execution
        .errors
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut response = json!({ "data": data });
    if !errors.is_empty() {
        response["errors"] = Value::Array(errors);
    }
    response
}

fn location(position: Position) -> Value {
    json!({"line": position.line, "column": position.column})
}

struct Validation<'a> {
    schema: &'static Schema,
    operation: &'a Operation,
    fragments: &'a HashMap<&'a str, &'a Fragment>,
    errors: Vec<Value>,
    validated: HashSet<&'a str>,
    stack: Vec<&'a str>,
}

impl<'a> Validation<'a> {
    fn error(&mut self, message: String, position: Position, code: &str, extra: Value) {
        if self.errors.len() >= MAX_ERRORS {
            return;
        }
        let mut extensions = json!({"code": code});
        if let (Some(extensions), Value::Object(extra)) = (extensions.as_object_mut(), extra) {
            extensions.extend(extra);
        }
        self.errors.push(json!({
            "message": message,
            "locations": [location(position)],
            "extensions": extensions,
        }));
    }

    fn selection_set(&mut self, parent: &str, selections: &'a [Selection]) {
        for selection in selections {
            match selection {
                Selection::Field(field) => self.field(parent, field),
                Selection::FragmentSpread { name, position, .. } => {
                    let Some(fragment) = self.fragments.get(name.as_str()).copied() else {
                        self.error(
                            format!("Fragment {name} was used, but not defined"),
                            *position,
                            "useAndDefineFragment",
                            json!({"fragmentName": name}),
                        );
                        continue;
                    };
                    if self.stack.contains(&fragment.name.as_str()) {
                        self.error(
                            format!("Fragment {name} contains an infinite loop"),
                            *position,
                            "infiniteLoop",
                            json!({"fragmentName": name}),
                        );
                        continue;
                    }
                    // Fragments fix their own type, so each body validates once.
                    if !self.validated.insert(fragment.name.as_str()) {
                        continue;
                    }
                    if self.schema.ty(&fragment.type_condition).is_some() {
                        self.stack.push(fragment.name.as_str());
                        self.selection_set(&fragment.type_condition, &fragment.selections);
                        self.stack.pop();
                    }
                }
                Selection::InlineFragment {
                    type_condition,
                    selections,
                    ..
                } => match type_condition {
                    // GitHub ignores conditions on types it does not define for
                    // this parent, so such a fragment simply never applies.
                    Some(condition) if self.schema.ty(condition).is_none() => {}
                    Some(condition) => self.selection_set(condition, selections),
                    None => self.selection_set(parent, selections),
                },
            }
        }
    }

    fn field(&mut self, parent: &str, field: &'a Field) {
        if field.name == "__typename" {
            if !field.selections.is_empty() {
                self.leaf_selection(field, "String!");
            }
            return;
        }
        let definition = match (parent, field.name.as_str()) {
            ("Query", "__schema") => None,
            ("Query", "__type") => None,
            _ => {
                let Some(definition) = self.schema.field(parent, &field.name) else {
                    self.error(
                        format!("Field '{}' doesn't exist on type '{parent}'", field.name),
                        field.position,
                        "undefinedField",
                        json!({"typeName": parent, "fieldName": field.name}),
                    );
                    return;
                };
                Some(definition)
            }
        };
        let (arguments, ty): (&[syntax::InputValueDefinition], String) = match definition {
            Some(definition) => (&definition.arguments, definition.ty.named().to_owned()),
            None if field.name == "__schema" => (&[], "__Schema".into()),
            None => (TYPE_ARGUMENTS.as_slice(), "__Type".into()),
        };
        for (name, value) in &field.arguments {
            if !arguments.iter().any(|argument| &argument.name == name) {
                self.error(
                    format!("Field '{}' doesn't accept argument '{name}'", field.name),
                    field.position,
                    "argumentNotAccepted",
                    json!({"name": field.name, "typeName": "Field", "argumentName": name}),
                );
            }
            self.variables(value, field.position);
        }
        let missing: Vec<&str> = arguments
            .iter()
            .filter(|argument| argument.ty.is_non_null() && argument.default.is_none())
            .filter(|argument| {
                !field
                    .arguments
                    .iter()
                    .any(|(name, _)| *name == argument.name)
            })
            .map(|argument| argument.name.as_str())
            .collect();
        if !missing.is_empty() {
            self.error(
                format!(
                    "Field '{}' is missing required arguments: {}",
                    field.name,
                    missing.join(", ")
                ),
                field.position,
                "missingRequiredArguments",
                json!({"className": "Field", "name": field.name, "arguments": missing.join(", ")}),
            );
        }
        if self.schema.is_composite(&ty) {
            if field.selections.is_empty() {
                self.error(
                    format!(
                        "Field must have selections (field '{}' returns {ty} but has no selections. Did you mean '{} {{ ... }}'?)",
                        field.name, field.name
                    ),
                    field.position,
                    "selectionMismatch",
                    json!({"nodeName": format!("field '{}'", field.name), "typeName": ty}),
                );
                return;
            }
            self.selection_set(&ty, &field.selections);
        } else if !field.selections.is_empty() {
            self.leaf_selection(field, &ty);
        }
    }

    fn leaf_selection(&mut self, field: &'a Field, ty: &str) {
        self.error(
            format!(
                "Selections can't be made on scalars (field '{}' returns {ty} but has selections)",
                field.name
            ),
            field.position,
            "selectionMismatch",
            json!({"nodeName": format!("field '{}'", field.name), "typeName": ty}),
        );
    }

    fn variables(&mut self, value: &Literal, position: Position) {
        match value {
            Literal::Variable(name) => {
                if !self
                    .operation
                    .variables
                    .iter()
                    .any(|definition| &definition.name == name)
                {
                    let operation = self.operation.name.as_deref().unwrap_or("anonymous query");
                    self.error(
                        format!("Variable ${name} is used by {operation} but not declared"),
                        position,
                        "variableNotDefined",
                        json!({"variableName": name}),
                    );
                }
            }
            Literal::List(items) => {
                for item in items {
                    self.variables(item, position);
                }
            }
            Literal::Object(fields) => {
                for (_, value) in fields {
                    self.variables(value, position);
                }
            }
            _ => {}
        }
    }
}

static TYPE_ARGUMENTS: std::sync::LazyLock<[syntax::InputValueDefinition; 1]> =
    std::sync::LazyLock::new(|| {
        [syntax::InputValueDefinition {
            name: "name".into(),
            ty: Type::NonNull(Box::new(Type::Named("String".into()))),
            default: None,
        }]
    });

fn coerce_variables(
    schema: &Schema,
    operation: &Operation,
    provided: &Map<String, Value>,
) -> Result<Map<String, Value>, Vec<Value>> {
    let mut variables = Map::new();
    let mut errors = Vec::new();
    for definition in &operation.variables {
        let value = match provided.get(&definition.name) {
            Some(value) => Some(value.clone()),
            None => definition.default.as_ref().and_then(constant),
        };
        match value {
            Some(Value::Null) | None if definition.ty.is_non_null() => errors.push(json!({
                "message": format!(
                    "Variable ${} of type {} was provided invalid value",
                    definition.name, definition.ty
                ),
                "locations": [location(operation.position)],
                "extensions": {"value": null, "problems": [{"path": [], "explanation": "Expected value to not be null"}]},
            })),
            Some(value) => {
                variables.insert(definition.name.clone(), coerce(schema, &definition.ty, value));
            }
            None => {}
        }
    }
    if errors.is_empty() {
        Ok(variables)
    } else {
        Err(errors)
    }
}

/// Applies input coercion that changes a value's shape: a single value given
/// for a list becomes a one-item list, recursively through input objects.
/// Scalar validation stays with resolvers, which own each argument's meaning.
fn coerce(schema: &Schema, ty: &Type, value: Value) -> Value {
    match (ty, value) {
        (Type::NonNull(inner), value) => coerce(schema, inner, value),
        (_, Value::Null) => Value::Null,
        (Type::List(inner), Value::Array(items)) => Value::Array(
            items
                .into_iter()
                .map(|item| coerce(schema, inner, item))
                .collect(),
        ),
        (Type::List(inner), value) => Value::Array(vec![coerce(schema, inner, value)]),
        (Type::Named(name), Value::Object(fields)) => {
            let Some(input) = schema.ty(name) else {
                return Value::Object(fields);
            };
            Value::Object(
                fields
                    .into_iter()
                    .map(|(field, value)| {
                        let value = match input.inputs.iter().find(|input| input.name == field) {
                            Some(definition) => coerce(schema, &definition.ty, value),
                            None => value,
                        };
                        (field, value)
                    })
                    .collect(),
            )
        }
        (Type::Named(_), value) => value,
    }
}

fn constant(value: &Literal) -> Option<Value> {
    convert(value, &Map::new())
}

/// Converts a literal to JSON. An unset variable is absent, not null, so
/// omitted input fields stay distinguishable from explicit nulls.
fn convert(value: &Literal, variables: &Map<String, Value>) -> Option<Value> {
    Some(match value {
        Literal::Variable(name) => return variables.get(name).cloned(),
        Literal::Int(value) => Value::from(*value),
        Literal::Float(value) => {
            serde_json::Number::from_f64(*value).map_or(Value::Null, Value::Number)
        }
        Literal::String(value) | Literal::Enum(value) => Value::String(value.clone()),
        Literal::Boolean(value) => Value::Bool(*value),
        Literal::Null => Value::Null,
        Literal::List(items) => Value::Array(
            items
                .iter()
                .map(|item| convert(item, variables).unwrap_or(Value::Null))
                .collect(),
        ),
        Literal::Object(fields) => Value::Object(
            fields
                .iter()
                .filter_map(|(name, value)| Some((name.clone(), convert(value, variables)?)))
                .collect(),
        ),
    })
}

enum Path<'p> {
    Root,
    Key(&'p Path<'p>, &'p str),
    Index(&'p Path<'p>, usize),
}

impl Path<'_> {
    fn to_json(&self) -> Vec<Value> {
        let mut segments = Vec::new();
        let mut current = self;
        loop {
            match current {
                Path::Root => break,
                Path::Key(parent, key) => {
                    segments.push(Value::String((*key).to_owned()));
                    current = parent;
                }
                Path::Index(parent, index) => {
                    segments.push(Value::from(*index));
                    current = parent;
                }
            }
        }
        segments.reverse();
        segments
    }
}

struct Execution<'a, C> {
    schema: &'static Schema,
    context: &'a C,
    fragments: &'a HashMap<&'a str, &'a Fragment>,
    variables: Map<String, Value>,
    errors: Mutex<Vec<Value>>,
    resolved: AtomicUsize,
}

impl<'a, C: Sync> Execution<'a, C> {
    fn record(&self, error: FieldError, path: &Path<'_>, position: Position) {
        let mut errors = self
            .errors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if errors.len() >= MAX_ERRORS {
            return;
        }
        let mut value = json!({
            "path": path.to_json(),
            "locations": [location(position)],
            "message": error.message,
        });
        if let Some(kind) = error.kind {
            value["type"] = Value::String(kind.into());
        }
        errors.push(value);
    }

    fn included(&self, directives: &[syntax::Directive]) -> bool {
        directives.iter().all(|directive| {
            let condition = directive
                .arguments
                .iter()
                .find(|(name, _)| name == "if")
                .and_then(|(_, value)| convert(value, &self.variables))
                .and_then(|value| value.as_bool());
            match directive.name.as_str() {
                "skip" => condition != Some(true),
                "include" => condition != Some(false),
                _ => true,
            }
        })
    }

    fn collect(
        &self,
        object_type: &str,
        selections: &'a [Selection],
        grouped: &mut Vec<(&'a str, Vec<&'a Field>)>,
        visited: &mut HashSet<&'a str>,
    ) {
        for selection in selections {
            match selection {
                Selection::Field(field) => {
                    if !self.included(&field.directives) {
                        continue;
                    }
                    let key = field.response_key();
                    match grouped.iter_mut().find(|(existing, _)| *existing == key) {
                        Some((_, fields)) => fields.push(field),
                        None => grouped.push((key, vec![field])),
                    }
                }
                Selection::FragmentSpread {
                    name, directives, ..
                } => {
                    if !self.included(directives) || !visited.insert(name.as_str()) {
                        continue;
                    }
                    if let Some(fragment) = self.fragments.get(name.as_str())
                        && self.schema.applies(&fragment.type_condition, object_type)
                    {
                        self.collect(object_type, &fragment.selections, grouped, visited);
                    }
                }
                Selection::InlineFragment {
                    type_condition,
                    directives,
                    selections,
                    ..
                } => {
                    let applies = type_condition
                        .as_deref()
                        .is_none_or(|condition| self.schema.applies(condition, object_type));
                    if applies && self.included(directives) {
                        self.collect(object_type, selections, grouped, visited);
                    }
                }
            }
        }
    }

    fn selection_set<'b>(
        &'b self,
        object: &'b dyn Object<C>,
        selections: &'b [&'a [Selection]],
        path: &'b Path<'b>,
    ) -> BoxFuture<'b, Option<Map<String, Value>>> {
        Box::pin(async move {
            let object_type = object.typename().to_owned();
            let mut grouped = Vec::new();
            let mut visited = HashSet::new();
            for selections in selections {
                self.collect(&object_type, selections, &mut grouped, &mut visited);
            }
            let mut map = Map::new();
            for (key, fields) in grouped {
                let first = fields[0];
                let field_path = Path::Key(path, key);
                if first.name == "__typename" {
                    map.insert(key.to_owned(), Value::String(object_type.clone()));
                    continue;
                }
                let (value, non_null) =
                    self.field(object, &object_type, &fields, &field_path).await;
                match value {
                    Some(value) => {
                        map.insert(key.to_owned(), value);
                    }
                    None if non_null => return None,
                    None => {
                        map.insert(key.to_owned(), Value::Null);
                    }
                }
            }
            Some(map)
        })
    }

    async fn field<'b>(
        &'b self,
        object: &'b dyn Object<C>,
        object_type: &str,
        fields: &[&'a Field],
        path: &'b Path<'b>,
    ) -> (Option<Value>, bool) {
        let first = fields[0];
        let definition = self.schema.field(object_type, &first.name);
        let arguments: Map<String, Value> = first
            .arguments
            .iter()
            .filter_map(|(name, value)| {
                let value = convert(value, &self.variables)?;
                let value = match definition.and_then(|field| {
                    field
                        .arguments
                        .iter()
                        .find(|argument| &argument.name == name)
                }) {
                    Some(argument) => coerce(self.schema, &argument.ty, value),
                    None => value,
                };
                Some((name.clone(), value))
            })
            .collect();
        let request = FieldRequest {
            name: &first.name,
            arguments,
            fields: fields.to_vec(),
            fragments: self.fragments,
        };
        let (resolved, ty): (Result<Resolved<C>, FieldError>, &'static Type) =
            match (object_type, first.name.as_str()) {
                ("Query", "__schema") => (
                    Ok(Resolved::object(SchemaObject(self.schema))),
                    &SCHEMA_TYPE,
                ),
                ("Query", "__type") => (
                    Ok(request
                        .string("name")
                        .map_or_else(Resolved::null, |name| TypeObject::named(self.schema, name))),
                    &TYPE_TYPE,
                ),
                _ => {
                    let Some(definition) = definition else {
                        // Validation saw a different static type than this value
                        // reports, which means the resolver broke its contract.
                        self.record(
                            FieldError::new(format!(
                                "Field '{}' is not available on {object_type}",
                                first.name
                            )),
                            path,
                            first.position,
                        );
                        return (None, false);
                    };
                    (object.resolve(self.context, &request).await, &definition.ty)
                }
            };
        match resolved {
            Ok(resolved) => (
                self.complete(ty, resolved, fields, path).await,
                ty.is_non_null(),
            ),
            Err(error) => {
                self.record(error, path, first.position);
                (None, ty.is_non_null())
            }
        }
    }

    fn complete<'b>(
        &'b self,
        ty: &'static Type,
        resolved: Resolved<C>,
        fields: &'b [&'a Field],
        path: &'b Path<'b>,
    ) -> BoxFuture<'b, Option<Value>> {
        Box::pin(async move {
            let position = fields[0].position;
            match ty {
                Type::NonNull(inner) => match self.complete(inner, resolved, fields, path).await {
                    Some(Value::Null) => {
                        self.record(
                            FieldError::new(format!(
                                "Cannot return null for non-nullable field {}",
                                fields[0].name
                            )),
                            path,
                            position,
                        );
                        None
                    }
                    value => value,
                },
                Type::List(inner) => match resolved {
                    Resolved::List(items) => {
                        let mut values = Vec::with_capacity(items.len());
                        for (index, item) in items.into_iter().enumerate() {
                            let item_path = Path::Index(path, index);
                            match self.complete(inner, item, fields, &item_path).await {
                                Some(value) => values.push(value),
                                None if inner.is_non_null() => return None,
                                None => values.push(Value::Null),
                            }
                        }
                        Some(Value::Array(values))
                    }
                    Resolved::Leaf(Value::Null) => Some(Value::Null),
                    // Leaf lists such as enum arrays arrive already encoded.
                    Resolved::Leaf(value @ Value::Array(_))
                        if !self.schema.is_composite(ty.named()) =>
                    {
                        Some(value)
                    }
                    _ => {
                        self.record(FieldError::new("Expected a list value"), path, position);
                        None
                    }
                },
                Type::Named(name) => match resolved {
                    Resolved::Leaf(Value::Null) => Some(Value::Null),
                    Resolved::Leaf(value) if !self.schema.is_composite(name) => Some(value),
                    Resolved::Object(object) if self.schema.is_composite(name) => {
                        if self.resolved.fetch_add(1, Ordering::Relaxed) >= MAX_RESOLVED_OBJECTS {
                            self.record(
                                FieldError::typed(
                                    "MAX_NODE_LIMIT_EXCEEDED",
                                    "This query requests too many objects. Request smaller pages.",
                                ),
                                path,
                                position,
                            );
                            return None;
                        }
                        let selections: Vec<&'a [Selection]> = fields
                            .iter()
                            .map(|field| field.selections.as_slice())
                            .collect();
                        self.selection_set(object.as_ref(), &selections, path)
                            .await
                            .map(Value::Object)
                    }
                    _ => {
                        self.record(
                            FieldError::new(format!("Resolver returned an invalid {name} value")),
                            path,
                            position,
                        );
                        None
                    }
                },
            }
        })
    }
}

static SCHEMA_TYPE: std::sync::LazyLock<Type> =
    std::sync::LazyLock::new(|| Type::NonNull(Box::new(Type::Named("__Schema".into()))));
static TYPE_TYPE: std::sync::LazyLock<Type> =
    std::sync::LazyLock::new(|| Type::Named("__Type".into()));
