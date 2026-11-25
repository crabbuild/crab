use std::collections::{BTreeMap, HashSet};

use super::*;

pub(crate) mod search;

const KEY: &str = "organization/v1/repository-properties";
const MAX_PROPERTIES: usize = 50;
pub(crate) const MAX_BATCH: usize = 50;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Kind {
    Text,
    Boolean,
    SingleSelect,
    MultiSelect,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EditAuthority {
    #[default]
    Organization,
    Repository,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub(crate) enum PropertyValue {
    Boolean(bool),
    Text(String),
    Options(Vec<String>),
}

impl PropertyValue {
    pub(super) fn matches(&self, term: &Self) -> bool {
        match (self, term) {
            (Self::Options(options), Self::Text(value)) => options.contains(value),
            (actual, expected) => actual == expected,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Definition {
    pub id: Uuid,
    pub name: String,
    pub description: String,
    pub kind: Kind,
    pub options: Vec<String>,
    pub default: Option<PropertyValue>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub values_editable_by: EditAuthority,
}

fn valid_text(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 75
        && value.trim() == value
        && value
            .bytes()
            .all(|byte| (b' '..=b'~').contains(&byte) && byte != b'"')
}

impl Definition {
    fn valid(&self) -> bool {
        let choices = matches!(self.kind, Kind::SingleSelect | Kind::MultiSelect);
        let mut unique = HashSet::new();
        let valid_options = if choices {
            !self.options.is_empty()
                && self.options.len() <= 50
                && self
                    .options
                    .iter()
                    .all(|value| valid_text(value) && unique.insert(value))
        } else {
            self.options.is_empty()
        };
        !self.id.is_nil()
            && !self.name.is_empty()
            && self.name.len() <= 75
            && self
                .name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-#$".contains(&byte))
            && self.description.len() <= 256
            && !self.description.chars().any(char::is_control)
            && valid_options
            && self
                .default
                .as_ref()
                .is_none_or(|value| self.accepts(value))
    }

    pub(crate) fn accepts(&self, value: &PropertyValue) -> bool {
        match (self.kind, value) {
            (Kind::Boolean, PropertyValue::Boolean(_)) => true,
            (Kind::Text, PropertyValue::Text(value)) => valid_text(value),
            (Kind::SingleSelect, PropertyValue::Text(value)) => self.options.contains(value),
            (Kind::MultiSelect, PropertyValue::Options(values)) => {
                let mut unique = HashSet::new();
                !values.is_empty()
                    && values.len() <= 20
                    && values
                        .iter()
                        .all(|value| self.options.contains(value) && unique.insert(value))
            }
            _ => false,
        }
    }

    pub(super) fn accepts_term(&self, value: &PropertyValue) -> bool {
        if self.kind == Kind::MultiSelect {
            return matches!(value, PropertyValue::Text(value) if self.options.contains(value));
        }
        self.accepts(value)
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct Schema {
    pub version: u64,
    pub definitions: Vec<Definition>,
}

impl Schema {
    pub(super) fn definition(&self, id: Uuid) -> Option<&Definition> {
        self.definitions
            .iter()
            .find(|definition| definition.id == id)
    }
}

pub(super) fn creation_fields(context: &Transaction<'_>, role: Role) -> Result<Vec<Definition>> {
    Ok(schema(context)?
        .definitions
        .into_iter()
        .filter(|field| {
            role == Role::Owner || field.values_editable_by == EditAuthority::Repository
        })
        .collect())
}

pub(super) fn validate_creation_values(
    context: &Transaction<'_>,
    role: Role,
    values: &BTreeMap<Uuid, PropertyValue>,
) -> Result<bool> {
    let fields = creation_fields(context, role)?;
    Ok(values.iter().all(|(id, value)| {
        fields
            .iter()
            .find(|field| field.id == *id)
            .is_some_and(|field| field.accepts(value))
    }))
}

pub(super) fn schema(context: &Transaction<'_>) -> Result<Schema> {
    Ok(context.document(KEY)?.unwrap_or_default())
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Repository {
    pub id: Uuid,
    pub ownership_epoch: u32,
}

impl Repository {
    fn key(self) -> Result<String> {
        if self.id.is_nil() {
            return Err(Error::Invalid("invalid property repository"));
        }
        Ok(format!(
            "{KEY}/repository/{}/{:08x}",
            self.id, self.ownership_epoch
        ))
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct Values {
    pub version: u64,
    pub overrides: BTreeMap<Uuid, PropertyValue>,
}

impl Values {
    pub(crate) fn effective<'a>(&'a self, definition: &'a Definition) -> Option<&'a PropertyValue> {
        self.overrides
            .get(&definition.id)
            .or(definition.default.as_ref())
    }
}

#[derive(Deserialize, Serialize)]
pub(crate) struct View {
    pub schema: Schema,
    pub values: Values,
    pub editable: Vec<Uuid>,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Editor {
    pub actor: Uuid,
    pub repository_admin: bool,
}

impl Editor {
    fn organization_owner(self, context: &Transaction<'_>) -> Result<bool> {
        actor(self.actor)?;
        Ok(member(context, self.actor)?
            .is_some_and(|member| member.active && member.role == Role::Owner))
    }
}

fn view(context: &Transaction<'_>, repository: Repository, editor: Option<Editor>) -> Result<View> {
    let schema = schema(context)?;
    let values = values(context, &schema, &repository.key()?)?;
    let mut editable = Vec::new();
    if let Some(editor) = editor {
        let owner = editor.organization_owner(context)?;
        editable.extend(schema.definitions.iter().filter_map(|definition| {
            (owner
                || editor.repository_admin
                    && definition.values_editable_by == EditAuthority::Repository)
                .then_some(definition.id)
        }));
    }
    Ok(View {
        schema,
        values,
        editable,
    })
}

fn values(context: &Transaction<'_>, schema: &Schema, key: &str) -> Result<Values> {
    let mut values: Values = context.document(key)?.unwrap_or_default();
    values
        .overrides
        .retain(|id, _| schema.definition(*id).is_some());
    Ok(values)
}

pub(super) fn effective(
    context: &Transaction<'_>,
    repository: Repository,
) -> Result<BTreeMap<Uuid, PropertyValue>> {
    let schema = schema(context)?;
    let values = values(context, &schema, &repository.key()?)?;
    Ok(schema
        .definitions
        .iter()
        .filter_map(|definition| {
            values
                .effective(definition)
                .map(|value| (definition.id, value.clone()))
        })
        .collect())
}

#[derive(Deserialize, Serialize)]
pub(crate) enum Outcome<T> {
    Saved(T),
    Conflict,
    Invalid,
    MissingRequiredDefault,
    Denied,
    UsedByRuleset,
    IncompatibleDefinition,
}

pub(crate) struct GetSchema;
impl Read for GetSchema {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 54;
    type Input = Caller;
    type Output = Schema;

    fn execute(context: &mut Transaction<'_>, _: Caller) -> Result<Schema> {
        schema(context)
    }
}

pub(crate) struct GetValues;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadValues {
    pub repository: Repository,
    pub editor: Option<Editor>,
}

impl Read for GetValues {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 55;
    type Input = ReadValues;
    type Output = View;

    fn execute(context: &mut Transaction<'_>, input: ReadValues) -> Result<View> {
        view(context, input.repository, input.editor)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetSchemaInput {
    pub actor: Uuid,
    pub expected_version: u64,
    pub definitions: Vec<Definition>,
}

pub(crate) struct SetSchema;
impl Mutation for SetSchema {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 54;
    type Input = SetSchemaInput;
    type Output = Outcome<Schema>;

    fn execute(
        context: &mut Transaction<'_>,
        input: SetSchemaInput,
    ) -> Result<MutationResult<Self::Output>> {
        let current = schema(context)?;
        if current.version != input.expected_version || current.version >= MAX_VERSION - 1 {
            return Ok(MutationResult::Rejected(Outcome::Conflict));
        }
        let mut ids = HashSet::new();
        let mut names = HashSet::new();
        if input.definitions.len() > MAX_PROPERTIES
            || serde_json::to_vec(&input.definitions)?.len() > 96 * 1024
            || !input.definitions.iter().all(|definition| {
                definition.valid()
                    && ids.insert(definition.id)
                    && names.insert(definition.name.to_ascii_lowercase())
            })
        {
            return Ok(MutationResult::Rejected(Outcome::Invalid));
        }
        if input
            .definitions
            .iter()
            .any(|definition| definition.required && definition.default.is_none())
        {
            return Ok(MutationResult::Rejected(Outcome::MissingRequiredDefault));
        }
        let next = Schema {
            version: current.version + 1,
            definitions: input.definitions,
        };
        if !super::rulesets::properties_valid(context, &next)? {
            return Ok(MutationResult::Rejected(Outcome::UsedByRuleset));
        }
        if current.definitions == next.definitions {
            return Ok(MutationResult::Success(Outcome::Saved(current)));
        }
        // A retained definition keeps all existing explicit values valid.
        // Defaults can change independently and apply to repositories with no override.
        if next.definitions.iter().any(|definition| {
            current.definition(definition.id).is_some_and(|old| {
                old.kind != definition.kind
                    || !old
                        .options
                        .iter()
                        .all(|value| definition.options.contains(value))
            })
        }) {
            return Ok(MutationResult::Rejected(Outcome::IncompatibleDefinition));
        }
        // Retired IDs cannot expose their old overrides if an API caller
        // submits a definition using the same ID again.
        let retired = |id| format!("{KEY}/retired/{id}");
        for definition in &next.definitions {
            if current.definition(definition.id).is_none()
                && context.document::<bool>(&retired(definition.id))?.is_some()
            {
                return Ok(MutationResult::Rejected(Outcome::Invalid));
            }
        }
        for definition in &current.definitions {
            if next.definition(definition.id).is_none() {
                context.put_document(&retired(definition.id), &true)?;
            }
        }
        context.put_document(KEY, &next)?;
        search::advance_revision(context)?;
        advance(
            context,
            input.actor,
            Action::RepositoryPropertiesChanged {
                repository: None,
                version: next.version,
            },
        )?;
        Ok(MutationResult::Success(Outcome::Saved(next)))
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetValuesInput {
    pub editor: Editor,
    pub repository: Repository,
    pub expected_version: u64,
    pub schema_version: u64,
    pub overrides: BTreeMap<Uuid, PropertyValue>,
}

pub(super) fn authorize_save(context: &Transaction<'_>, input: &Value) -> Result<()> {
    #[derive(Deserialize)]
    struct Caller {
        editor: Editor,
    }
    let Caller { editor } = serde_json::from_value(input.clone())?;
    actor(editor.actor)?;
    if editor.repository_admin || editor.organization_owner(context)? {
        return Ok(());
    }
    Err(Error::AccessDenied)
}

pub(crate) struct SetValues;
impl Mutation for SetValues {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 55;
    type Input = SetValuesInput;
    type Output = Outcome<View>;

    fn execute(
        context: &mut Transaction<'_>,
        input: SetValuesInput,
    ) -> Result<MutationResult<Self::Output>> {
        let mut current = view(context, input.repository, Some(input.editor))?;
        if current.schema.version != input.schema_version
            || current.values.version != input.expected_version
            || current.values.version >= MAX_VERSION - 1
        {
            return Ok(MutationResult::Rejected(Outcome::Conflict));
        }
        if !input.overrides.iter().all(|(id, value)| {
            current
                .schema
                .definition(*id)
                .is_some_and(|definition| definition.accepts(value))
        }) {
            return Ok(MutationResult::Rejected(Outcome::Invalid));
        }
        // A repository administrator submits a full snapshot. Compare omissions
        // as well as supplied entries so locked overrides cannot be cleared.
        if current
            .values
            .overrides
            .keys()
            .chain(input.overrides.keys())
            .any(|id| {
                current.values.overrides.get(id) != input.overrides.get(id)
                    && !current.editable.contains(id)
            })
        {
            return Ok(MutationResult::Rejected(Outcome::Denied));
        }
        if current.values.overrides == input.overrides {
            return Ok(MutationResult::Success(Outcome::Saved(current)));
        }
        current.values.overrides = input.overrides;
        save_values(
            context,
            input.editor.actor,
            input.repository,
            &mut current.values,
        )?;
        search::advance_revision(context)?;
        Ok(MutationResult::Success(Outcome::Saved(current)))
    }
}

fn save_values(
    context: &Transaction<'_>,
    actor: Uuid,
    repository: Repository,
    values: &mut Values,
) -> Result<()> {
    values.version += 1;
    context.put_document(&repository.key()?, values)?;
    advance(
        context,
        actor,
        Action::RepositoryPropertiesChanged {
            repository: Some(repository.id),
            version: values.version,
        },
    )
}

pub(super) fn initialize_values(
    context: &Transaction<'_>,
    actor: Uuid,
    repository: Repository,
    overrides: BTreeMap<Uuid, PropertyValue>,
) -> Result<()> {
    let key = repository.key()?;
    // A retry can cross a schema edit. Retired IDs stay retired, and an
    // already applied initialization must never overwrite later edits.
    if context.document::<Values>(&key)?.is_some() {
        return Ok(());
    }
    let schema = schema(context)?;
    let mut values = Values {
        version: 0,
        overrides,
    };
    values
        .overrides
        .retain(|id, _| schema.definition(*id).is_some());
    if !values.overrides.is_empty() {
        save_values(context, actor, repository, &mut values)?;
        search::advance_revision(context)?;
    }
    Ok(())
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ImportValues {
    pub actor: Uuid,
    pub repository: Uuid,
    pub overrides: BTreeMap<Uuid, PropertyValue>,
}

pub(crate) struct InitializeImportValues;
impl Mutation for InitializeImportValues {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 99;
    type Input = ImportValues;
    type Output = ();

    fn execute(context: &mut Transaction<'_>, input: ImportValues) -> Result<MutationResult<()>> {
        let repository = Repository {
            id: input.repository,
            ownership_epoch: 0,
        };
        initialize_values(context, input.actor, repository, input.overrides)?;
        Ok(MutationResult::Success(()))
    }
}

fn valid_selection(ids: impl ExactSizeIterator<Item = Uuid>) -> bool {
    let mut unique = HashSet::new();
    let mut ids = ids;
    (1..=MAX_BATCH).contains(&ids.len()) && ids.all(|id| !id.is_nil() && unique.insert(id))
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VersionedRepository {
    pub repository: Repository,
    pub version: u64,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum SelectionValue {
    Shared { value: Option<PropertyValue> },
    Mixed,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct SelectionView {
    pub schema: Schema,
    pub repositories: Vec<VersionedRepository>,
    pub effective: BTreeMap<Uuid, SelectionValue>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Selection {
    pub actor: Uuid,
    pub repositories: Vec<Repository>,
}

pub(crate) struct GetSelection;
impl Read for GetSelection {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 56;
    type Input = Selection;
    type Output = SelectionView;

    fn execute(context: &mut Transaction<'_>, selection: Selection) -> Result<SelectionView> {
        if !valid_selection(selection.repositories.iter().map(|repo| repo.id)) {
            return Err(Error::Invalid("invalid repository property selection"));
        }
        let schema = schema(context)?;
        let mut repositories = Vec::with_capacity(selection.repositories.len());
        let mut effective = BTreeMap::new();
        for repository in selection.repositories {
            let current = values(context, &schema, &repository.key()?)?;
            repositories.push(VersionedRepository {
                repository,
                version: current.version,
            });
            for definition in &schema.definitions {
                let value = current.effective(definition);
                match effective.entry(definition.id) {
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        entry.insert(SelectionValue::Shared {
                            value: value.cloned(),
                        });
                    }
                    std::collections::btree_map::Entry::Occupied(mut entry) => {
                        if matches!(entry.get(), SelectionValue::Shared { value: previous } if previous.as_ref() != value)
                        {
                            entry.insert(SelectionValue::Mixed);
                        }
                    }
                }
            }
        }
        Ok(SelectionView {
            schema,
            repositories,
            effective,
        })
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdateSelection {
    pub actor: Uuid,
    pub schema_version: u64,
    pub repositories: Vec<VersionedRepository>,
    pub changes: BTreeMap<Uuid, Option<PropertyValue>>,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct SelectionReceipt {
    pub updated: usize,
}

pub(crate) struct SetSelection;
impl Mutation for SetSelection {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 56;
    type Input = UpdateSelection;
    type Output = Outcome<SelectionReceipt>;

    fn execute(
        context: &mut Transaction<'_>,
        input: UpdateSelection,
    ) -> Result<MutationResult<Self::Output>> {
        let schema = schema(context)?;
        if schema.version != input.schema_version {
            return Ok(MutationResult::Rejected(Outcome::Conflict));
        }
        if !valid_selection(input.repositories.iter().map(|row| row.repository.id))
            || input.changes.is_empty()
            || !input.changes.iter().all(|(id, value)| {
                schema.definition(*id).is_some_and(|definition| {
                    value.as_ref().is_none_or(|value| definition.accepts(value))
                })
            })
        {
            return Ok(MutationResult::Rejected(Outcome::Invalid));
        }
        // Collect every valid edit before writing. A changed schema or a stale
        // repository leaves all selected values and audit events untouched.
        let mut pending = Vec::new();
        for row in input.repositories {
            let mut current = values(context, &schema, &row.repository.key()?)?;
            if row.version != current.version || current.version >= MAX_VERSION - 1 {
                return Ok(MutationResult::Rejected(Outcome::Conflict));
            }
            let changes = input
                .changes
                .iter()
                .any(|(id, value)| current.overrides.get(id) != value.as_ref());
            if changes {
                for (id, value) in &input.changes {
                    match value {
                        Some(value) => {
                            current.overrides.insert(*id, value.clone());
                        }
                        None => {
                            current.overrides.remove(id);
                        }
                    }
                }
                pending.push((row.repository, current));
            }
        }
        let updated = pending.len();
        for (repository, mut values) in pending {
            save_values(context, input.actor, repository, &mut values)?;
        }
        if updated != 0 {
            search::advance_revision(context)?;
        }
        Ok(MutationResult::Success(Outcome::Saved(SelectionReceipt {
            updated,
        })))
    }
}
