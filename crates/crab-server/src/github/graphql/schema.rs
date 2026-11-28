//! Schema model built from SDL, plus the specification introspection objects.

use std::collections::HashMap;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::syntax::{
    FieldDefinition, InputValueDefinition, SyntaxError, Type, TypeDefinition, TypeKind,
    parse_schema,
};
use super::{FieldError, FieldRequest, Object, Resolved};

const BUILTIN: &str = r#"
scalar String
scalar Int
scalar Float
scalar Boolean
scalar ID

type __Schema {
  description: String
  types: [__Type!]!
  queryType: __Type!
  mutationType: __Type
  subscriptionType: __Type
  directives: [__Directive!]!
}

type __Type {
  kind: __TypeKind!
  name: String
  description: String
  specifiedByURL: String
  fields(includeDeprecated: Boolean = false): [__Field!]
  interfaces: [__Type!]
  possibleTypes: [__Type!]
  enumValues(includeDeprecated: Boolean = false): [__EnumValue!]
  inputFields(includeDeprecated: Boolean = false): [__InputValue!]
  ofType: __Type
  isOneOf: Boolean
}

type __Field {
  name: String!
  description: String
  args(includeDeprecated: Boolean = false): [__InputValue!]!
  type: __Type!
  isDeprecated: Boolean!
  deprecationReason: String
}

type __InputValue {
  name: String!
  description: String
  type: __Type!
  defaultValue: String
  isDeprecated: Boolean!
  deprecationReason: String
}

type __EnumValue {
  name: String!
  description: String
  isDeprecated: Boolean!
  deprecationReason: String
}

type __Directive {
  name: String!
  description: String
  locations: [__DirectiveLocation!]!
  args(includeDeprecated: Boolean = false): [__InputValue!]!
  isRepeatable: Boolean!
}

enum __TypeKind { SCALAR OBJECT INTERFACE UNION ENUM INPUT_OBJECT LIST NON_NULL }

enum __DirectiveLocation {
  QUERY MUTATION SUBSCRIPTION FIELD FRAGMENT_DEFINITION FRAGMENT_SPREAD INLINE_FRAGMENT
  VARIABLE_DEFINITION SCHEMA SCALAR OBJECT FIELD_DEFINITION ARGUMENT_DEFINITION INTERFACE
  UNION ENUM ENUM_VALUE INPUT_OBJECT INPUT_FIELD_DEFINITION
}
"#;

pub(crate) struct Schema {
    types: Vec<TypeDefinition>,
    index: HashMap<String, usize>,
    possible: HashMap<String, Vec<String>>,
}

impl Schema {
    /// Parses `source` together with the built-in scalars and introspection
    /// types, then checks every type reference so execution can rely on it.
    pub(crate) fn parse(source: &str) -> Result<Self, SyntaxError> {
        let mut types = parse_schema(BUILTIN)?;
        types.extend(parse_schema(source)?);
        let invalid = |message: String| SyntaxError {
            message,
            position: Default::default(),
        };
        let mut index = HashMap::new();
        for (position, ty) in types.iter().enumerate() {
            if index.insert(ty.name.clone(), position).is_some() {
                return Err(invalid(format!("Type {} is defined twice", ty.name)));
            }
        }
        let mut possible: HashMap<String, Vec<String>> = HashMap::new();
        for ty in &types {
            for interface in &ty.interfaces {
                possible
                    .entry(interface.clone())
                    .or_default()
                    .push(ty.name.clone());
            }
            if ty.kind == TypeKind::Union {
                possible.insert(ty.name.clone(), ty.members.clone());
            }
        }
        let schema = Self {
            types,
            index,
            possible,
        };
        for ty in &schema.types {
            let references = ty
                .fields
                .iter()
                .flat_map(|field| {
                    std::iter::once(field.ty.named())
                        .chain(field.arguments.iter().map(|argument| argument.ty.named()))
                })
                .chain(ty.inputs.iter().map(|input| input.ty.named()))
                .chain(ty.interfaces.iter().map(String::as_str))
                .chain(ty.members.iter().map(String::as_str));
            for reference in references {
                if !schema.index.contains_key(reference) {
                    return Err(invalid(format!(
                        "Type {} references undefined type {reference}",
                        ty.name
                    )));
                }
            }
        }
        if schema.ty("Query").is_none() {
            return Err(invalid("Schema has no Query type".into()));
        }
        Ok(schema)
    }

    pub(crate) fn ty(&self, name: &str) -> Option<&TypeDefinition> {
        self.index.get(name).map(|position| &self.types[*position])
    }

    pub(crate) fn field(&self, type_name: &str, field: &str) -> Option<&FieldDefinition> {
        self.ty(type_name)?
            .fields
            .iter()
            .find(|candidate| candidate.name == field)
    }

    pub(crate) fn is_composite(&self, name: &str) -> bool {
        self.ty(name).is_some_and(|ty| {
            matches!(
                ty.kind,
                TypeKind::Object | TypeKind::Interface | TypeKind::Union
            )
        })
    }

    /// Whether a fragment with `condition` applies to a value of `object`.
    pub(crate) fn applies(&self, condition: &str, object: &str) -> bool {
        condition == object
            || self
                .possible
                .get(condition)
                .is_some_and(|members| members.iter().any(|member| member == object))
    }

    pub(crate) fn mutation(&self) -> Option<&TypeDefinition> {
        self.ty("Mutation")
    }
}

/// Root object for the `__schema` field.
pub(crate) struct SchemaObject(pub(crate) &'static Schema);

#[async_trait]
impl<C: Sync> Object<C> for SchemaObject {
    fn typename(&self) -> &str {
        "__Schema"
    }

    async fn resolve(&self, _: &C, field: &FieldRequest<'_>) -> Result<Resolved<C>, FieldError> {
        let schema = self.0;
        Ok(match field.name {
            "types" => Resolved::List(
                schema
                    .types
                    .iter()
                    .map(|ty| TypeObject::named(schema, &ty.name))
                    .collect(),
            ),
            "queryType" => TypeObject::named(schema, "Query"),
            "mutationType" => match schema.mutation() {
                Some(_) => TypeObject::named(schema, "Mutation"),
                None => Resolved::null(),
            },
            "directives" => Resolved::List(
                ["include", "skip"]
                    .into_iter()
                    .map(|name| Resolved::object(DirectiveObject(name)))
                    .collect(),
            ),
            _ => Resolved::null(),
        })
    }
}

#[derive(Clone)]
pub(crate) enum TypeObject {
    Named(&'static Schema, &'static TypeDefinition),
    List(&'static Schema, &'static Type),
    NonNull(&'static Schema, &'static Type),
}

impl TypeObject {
    pub(crate) fn named<C: Sync>(schema: &'static Schema, name: &str) -> Resolved<C> {
        match schema.ty(name) {
            Some(ty) => Resolved::object(Self::Named(schema, ty)),
            None => Resolved::null(),
        }
    }

    fn reference<C: Sync>(schema: &'static Schema, ty: &'static Type) -> Resolved<C> {
        match ty {
            Type::Named(name) => Self::named(schema, name),
            Type::List(inner) => Resolved::object(Self::List(schema, inner)),
            Type::NonNull(inner) => Resolved::object(Self::NonNull(schema, inner)),
        }
    }
}

#[async_trait]
impl<C: Sync> Object<C> for TypeObject {
    fn typename(&self) -> &str {
        "__Type"
    }

    async fn resolve(&self, _: &C, field: &FieldRequest<'_>) -> Result<Resolved<C>, FieldError> {
        let (schema, definition) = match self {
            Self::List(schema, inner) | Self::NonNull(schema, inner) => {
                return Ok(match field.name {
                    "kind" => Resolved::leaf(if matches!(self, Self::List(..)) {
                        "LIST"
                    } else {
                        "NON_NULL"
                    }),
                    "ofType" => TypeObject::reference(schema, inner),
                    _ => Resolved::null(),
                });
            }
            Self::Named(schema, definition) => (*schema, *definition),
        };
        let kind = definition.kind;
        Ok(match field.name {
            "kind" => Resolved::leaf(match kind {
                TypeKind::Scalar => "SCALAR",
                TypeKind::Object => "OBJECT",
                TypeKind::Interface => "INTERFACE",
                TypeKind::Union => "UNION",
                TypeKind::Enum => "ENUM",
                TypeKind::InputObject => "INPUT_OBJECT",
            }),
            "name" => Resolved::leaf(definition.name.as_str()),
            "fields" if matches!(kind, TypeKind::Object | TypeKind::Interface) => Resolved::List(
                definition
                    .fields
                    .iter()
                    .map(|field| Resolved::object(FieldObject(schema, field)))
                    .collect(),
            ),
            "interfaces" if kind == TypeKind::Object || kind == TypeKind::Interface => {
                Resolved::List(
                    definition
                        .interfaces
                        .iter()
                        .map(|name| TypeObject::named(schema, name))
                        .collect(),
                )
            }
            "possibleTypes" if matches!(kind, TypeKind::Interface | TypeKind::Union) => {
                Resolved::List(
                    schema
                        .possible
                        .get(&definition.name)
                        .into_iter()
                        .flatten()
                        .map(|name| TypeObject::named(schema, name))
                        .collect(),
                )
            }
            "enumValues" if kind == TypeKind::Enum => Resolved::List(
                definition
                    .values
                    .iter()
                    .map(|value| Resolved::object(EnumValueObject(value)))
                    .collect(),
            ),
            "inputFields" if kind == TypeKind::InputObject => Resolved::List(
                definition
                    .inputs
                    .iter()
                    .map(|input| Resolved::object(InputValueObject(schema, input)))
                    .collect(),
            ),
            "isOneOf" if kind == TypeKind::InputObject => Resolved::leaf(false),
            _ => Resolved::null(),
        })
    }
}

struct FieldObject(&'static Schema, &'static FieldDefinition);

#[async_trait]
impl<C: Sync> Object<C> for FieldObject {
    fn typename(&self) -> &str {
        "__Field"
    }

    async fn resolve(&self, _: &C, field: &FieldRequest<'_>) -> Result<Resolved<C>, FieldError> {
        let Self(schema, definition) = self;
        Ok(match field.name {
            "name" => Resolved::leaf(definition.name.as_str()),
            "args" => Resolved::List(
                definition
                    .arguments
                    .iter()
                    .map(|argument| Resolved::object(InputValueObject(schema, argument)))
                    .collect(),
            ),
            "type" => TypeObject::reference(schema, &definition.ty),
            "isDeprecated" => Resolved::leaf(false),
            _ => Resolved::null(),
        })
    }
}

struct InputValueObject(&'static Schema, &'static InputValueDefinition);

#[async_trait]
impl<C: Sync> Object<C> for InputValueObject {
    fn typename(&self) -> &str {
        "__InputValue"
    }

    async fn resolve(&self, _: &C, field: &FieldRequest<'_>) -> Result<Resolved<C>, FieldError> {
        let Self(schema, definition) = self;
        Ok(match field.name {
            "name" => Resolved::leaf(definition.name.as_str()),
            "type" => TypeObject::reference(schema, &definition.ty),
            "defaultValue" => match &definition.default {
                Some(value) => Resolved::Leaf(Value::String(print_value(value))),
                None => Resolved::null(),
            },
            "isDeprecated" => Resolved::leaf(false),
            _ => Resolved::null(),
        })
    }
}

struct EnumValueObject(&'static str);

#[async_trait]
impl<C: Sync> Object<C> for EnumValueObject {
    fn typename(&self) -> &str {
        "__EnumValue"
    }

    async fn resolve(&self, _: &C, field: &FieldRequest<'_>) -> Result<Resolved<C>, FieldError> {
        Ok(match field.name {
            "name" => Resolved::leaf(self.0),
            "isDeprecated" => Resolved::leaf(false),
            _ => Resolved::null(),
        })
    }
}

struct DirectiveObject(&'static str);

#[async_trait]
impl<C: Sync> Object<C> for DirectiveObject {
    fn typename(&self) -> &str {
        "__Directive"
    }

    async fn resolve(&self, _: &C, field: &FieldRequest<'_>) -> Result<Resolved<C>, FieldError> {
        Ok(match field.name {
            "name" => Resolved::leaf(self.0),
            "locations" => Resolved::Leaf(json!(["FIELD", "FRAGMENT_SPREAD", "INLINE_FRAGMENT"])),
            // Arguments of the built-in directives are not addressable schema
            // definitions; an empty list keeps introspection well-formed.
            "args" => Resolved::List(Vec::new()),
            "isRepeatable" => Resolved::leaf(false),
            _ => Resolved::null(),
        })
    }
}

fn print_value(value: &super::syntax::Value) -> String {
    use super::syntax::Value as Literal;
    match value {
        Literal::Variable(name) => format!("${name}"),
        Literal::Int(value) => value.to_string(),
        Literal::Float(value) => value.to_string(),
        Literal::String(value) => Value::String(value.clone()).to_string(),
        Literal::Boolean(value) => value.to_string(),
        Literal::Null => "null".into(),
        Literal::Enum(value) => value.clone(),
        Literal::List(items) => format!(
            "[{}]",
            items.iter().map(print_value).collect::<Vec<_>>().join(", ")
        ),
        Literal::Object(fields) => format!(
            "{{{}}}",
            fields
                .iter()
                .map(|(name, value)| format!("{name}: {}", print_value(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}
