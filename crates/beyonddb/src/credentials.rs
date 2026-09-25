//! Encrypted, access-key-sharded credentials for ExtendDB SigV4 verification.

use std::sync::OnceLock;

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use crab_cell_app::CellType;
use crab_cell_runtime::cell::catalog::{CatalogRole, CellCatalog};
use crab_cell_runtime::client::{CellClient, InvocationError};
use crab_cell_runtime::identity::{CellTarget, Digest, NamespaceId, TenantId};
use crab_cell_runtime::ltx::CellStorageLayout;
use crab_cell_runtime::registry::{
    Command, CommandContext, CommandResult, MigrationDescriptor, ModuleDescriptor,
    NamespaceDescriptor, OperationDescriptor, Query, QueryContext, RegistryBuilder,
};
use extenddb_auth::{CredentialStore, StoredCredential};
use extenddb_core::error::DynamoDbError;
use extenddb_storage::error::StorageError;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::backend::{cell_error, mutation_identity};
use crate::table::statement;
use crate::{APPLICATION, Error, Json, OPERATION_BYTES, Result, SqlValue};

pub(crate) const MODULE: &str = "beyonddb-credentials";
pub(crate) const NAMESPACE: NamespaceId = NamespaceId::from_bytes([0x44; 16]);
const TENANT: TenantId = TenantId::from_bytes([0x44; 16]);
const SHARDS: u32 = 256;
const SCHEMA: &str = include_str!("credential_schema.sql");

static NAMESPACES: [NamespaceDescriptor; 1] = [NamespaceDescriptor {
    id: NAMESPACE,
    name: MODULE,
    role: CatalogRole::Sql,
    shards: SHARDS,
    effect_targets: &[],
    dead_letter: None,
}];
static COMMANDS: [OperationDescriptor; 2] = [operation(1), operation(2)];
static QUERIES: [OperationDescriptor; 1] = [operation(1)];

const fn operation(id: u32) -> OperationDescriptor {
    OperationDescriptor {
        id,
        codec_version: 1,
        schema_min: 1,
        schema_max: 1,
        input_limit: OPERATION_BYTES,
        output_limit: OPERATION_BYTES,
    }
}

pub(crate) struct CredentialModule;

impl crab_cell_runtime::registry::CellModule for CredentialModule {
    const NAME: &'static str = MODULE;

    fn descriptor(&self) -> &'static ModuleDescriptor {
        static DESCRIPTOR: OnceLock<ModuleDescriptor> = OnceLock::new();
        DESCRIPTOR.get_or_init(|| ModuleDescriptor {
            name: MODULE,
            source_digest: Digest::from_bytes(
                *blake3::hash(include_bytes!("credentials.rs")).as_bytes(),
            ),
            retained_codes: &[],
            schema_min: 1,
            schema_max: 1,
            migrations: Box::leak(Box::new([MigrationDescriptor {
                version: 1,
                sql: SCHEMA,
                digest: Digest::from_bytes(*blake3::hash(SCHEMA.as_bytes()).as_bytes()),
            }])),
            commands: &COMMANDS,
            queries: &QUERIES,
            workflow_definitions: &[],
            activity_types: &[],
            namespaces: &NAMESPACES,
        })
    }

    fn register(self, registry: &mut RegistryBuilder) -> Result<()> {
        registry.bind_command::<PutCredential>()?;
        registry.bind_command::<RevokeCredential>()?;
        registry.bind_query::<ReadCredential>()
    }
}

pub(crate) fn cell_type() -> Result<CellType> {
    CellType::new(MODULE, MODULE, NAMESPACE, CatalogRole::Sql, SHARDS)?
        .with_limits(512 * 1024 * 1024, 64 * 1024 * 1024)
}

/// Map one access key to its stable credential Cell.
pub fn credential_target(access_key_id: &str) -> Result<CellTarget> {
    if access_key_id.is_empty()
        || access_key_id.len() > 128
        || !access_key_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric())
    {
        return Err(Error::Identity("invalid access key ID"));
    }
    CellTarget::new(
        TENANT,
        APPLICATION,
        NAMESPACE,
        &cell_type()?.partition_for_scope(access_key_id.as_bytes())?,
    )
}

/// Install the credential shard schema during Cell bootstrap.
pub fn initialize_credentials(transaction: &crab_ltx::rusqlite::Transaction<'_>) -> Result<()> {
    transaction.execute_batch(SCHEMA)?;
    Ok(())
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct CredentialRecord {
    access_key_id: String,
    account_id: String,
    principal_name: String,
    is_active: bool,
    encrypted_secret: Vec<u8>,
}

/// Put a new encrypted credential in its key-derived shard.
pub(crate) struct PutCredential;

/// Result of inserting one access key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum PutCredentialOutcome {
    /// A new credential was committed.
    Created,
    /// The key ID already exists and was not overwritten.
    AlreadyExists,
    /// The request was sent to the wrong shard.
    WrongShard,
}

impl Command for PutCredential {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 1;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<CredentialRecord>;
    type Output = Json<PutCredentialOutcome>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(record): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        if credential_target(&record.access_key_id)? != *context.target() {
            return Ok(CommandResult::Rejected(Json(
                PutCredentialOutcome::WrongShard,
            )));
        }
        let existing = context.sql(&statement(
            "SELECT 1 FROM ddb_credentials WHERE access_key_id = ?1",
            vec![SqlValue::Text(record.access_key_id.clone())],
        ))?;
        if !existing[0].rows.is_empty() {
            return Ok(CommandResult::Rejected(Json(
                PutCredentialOutcome::AlreadyExists,
            )));
        }
        context.sql(&statement(
            "INSERT INTO ddb_credentials (access_key_id, record) VALUES (?1, ?2)",
            vec![
                SqlValue::Text(record.access_key_id.clone()),
                SqlValue::Blob(serde_json::to_vec(&record)?),
            ],
        ))?;
        Ok(CommandResult::Success(Json(PutCredentialOutcome::Created)))
    }
}

/// Disable an access key in the same Cell that authenticates it.
pub(crate) struct RevokeCredential;

impl Command for RevokeCredential {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 2;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<String>;
    type Output = Json<bool>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(key_id): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        if credential_target(&key_id)? != *context.target() {
            return Err(Error::Identity(
                "credential revocation reached the wrong shard",
            ));
        }
        let result = context.sql(&statement(
            "SELECT record FROM ddb_credentials WHERE access_key_id = ?1",
            vec![SqlValue::Text(key_id.clone())],
        ))?;
        let Some(row) = result[0].rows.first() else {
            return Ok(CommandResult::Success(Json(false)));
        };
        let [SqlValue::Blob(bytes)] = row.as_slice() else {
            return Err(Error::Command("invalid credential row"));
        };
        let mut record: CredentialRecord = serde_json::from_slice(bytes)?;
        if record.access_key_id != key_id {
            return Err(Error::Command("credential row key mismatch"));
        }
        if record.is_active {
            record.is_active = false;
            context.sql(&statement(
                "UPDATE ddb_credentials SET record = ?2 WHERE access_key_id = ?1",
                vec![
                    SqlValue::Text(key_id),
                    SqlValue::Blob(serde_json::to_vec(&record)?),
                ],
            ))?;
        }
        Ok(CommandResult::Success(Json(true)))
    }
}

/// Read a credential from its key-derived shard.
pub(crate) struct ReadCredential;

impl Query for ReadCredential {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 1;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<String>;
    type Output = Json<Option<CredentialRecord>>;

    fn execute(context: &mut QueryContext<'_>, Json(key_id): Self::Input) -> Result<Self::Output> {
        if credential_target(&key_id)?.cell_id() != context.cell_id() {
            return Err(Error::Identity("credential query reached the wrong shard"));
        }
        let result = context.sql(&statement(
            "SELECT record FROM ddb_credentials WHERE access_key_id = ?1",
            vec![SqlValue::Text(key_id.clone())],
        ))?;
        let Some(row) = result[0].rows.first() else {
            return Ok(Json(None));
        };
        let [SqlValue::Blob(bytes)] = row.as_slice() else {
            return Err(Error::Command("invalid credential row"));
        };
        let record: CredentialRecord = serde_json::from_slice(bytes)?;
        if record.access_key_id != key_id {
            return Err(Error::Command("credential row key mismatch"));
        }
        Ok(Json(Some(record)))
    }
}

/// Encrypted credential lookup over published credential Cells.
pub struct CellCredentialStore {
    client: CellClient,
    layout: CellStorageLayout,
    encryption_key: Zeroizing<[u8; 32]>,
}

impl CellCredentialStore {
    /// Bind a Cell client and an operator-supplied AES-256 key.
    pub fn new(client: CellClient, layout: CellStorageLayout, encryption_key: [u8; 32]) -> Self {
        Self {
            client,
            layout,
            encryption_key: Zeroizing::new(encryption_key),
        }
    }

    /// Encrypt and publish a new credential without sending its secret to a Cell.
    pub async fn put_credential(
        &self,
        access_key_id: &str,
        credential: StoredCredential,
    ) -> std::result::Result<(), StorageError> {
        crate::account_target(&credential.account_id)
            .map_err(|error| StorageError::Validation(error.to_string()))?;
        if credential.is_session
            || credential.session_name.is_some()
            || credential.session_token.is_some()
            || credential.expires_at.is_some()
        {
            return Err(StorageError::Unsupported(
                "temporary session credentials".into(),
            ));
        }
        let target = credential_target(access_key_id)
            .map_err(|error| StorageError::Validation(error.to_string()))?;
        let mut nonce = [0_u8; 12];
        getrandom::fill(&mut nonce).map_err(|error| StorageError::Internal(error.to_string()))?;
        let cipher = Aes256Gcm::new_from_slice(&*self.encryption_key)
            .map_err(|_| StorageError::Internal("invalid credential encryption key".into()))?;
        let encrypted = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: credential.secret_key.as_bytes(),
                    aad: access_key_id.as_bytes(),
                },
            )
            .map_err(|_| StorageError::Internal("credential encryption failed".into()))?;
        let mut encrypted_secret = nonce.to_vec();
        encrypted_secret.extend_from_slice(&encrypted);
        let record = CredentialRecord {
            access_key_id: access_key_id.into(),
            account_id: credential.account_id.clone(),
            principal_name: credential.principal_name.clone(),
            is_active: credential.is_active,
            encrypted_secret,
        };
        match self
            .client
            .command::<PutCredential>(&target, mutation_identity()?, Json(record))
            .await
        {
            Ok(committed) if committed.output.0 == PutCredentialOutcome::Created => Ok(()),
            Ok(_) => Err(StorageError::Internal(
                "unexpected credential result".into(),
            )),
            Err(InvocationError::Rejected(committed)) => match committed.output.0 {
                PutCredentialOutcome::AlreadyExists => {
                    Err(StorageError::Validation("access key already exists".into()))
                }
                PutCredentialOutcome::WrongShard => {
                    Err(StorageError::Internal("credential shard mismatch".into()))
                }
                PutCredentialOutcome::Created => Err(StorageError::Internal(
                    "unexpected credential rejection".into(),
                )),
            },
            Err(error) => Err(cell_error(error)),
        }
    }

    /// Disable an existing access key durably; repeat calls are idempotent.
    pub async fn revoke_credential(
        &self,
        access_key_id: &str,
    ) -> std::result::Result<bool, StorageError> {
        let target = credential_target(access_key_id)
            .map_err(|error| StorageError::Validation(error.to_string()))?;
        let result = self
            .client
            .command::<RevokeCredential>(&target, mutation_identity()?, Json(access_key_id.into()))
            .await
            .map_err(cell_error)?;
        Ok(result.output.0)
    }

    async fn read(
        &self,
        access_key_id: &str,
    ) -> std::result::Result<Option<StoredCredential>, DynamoDbError> {
        let target = credential_target(access_key_id)
            .map_err(|_| DynamoDbError::UnrecognizedClientException("invalid access key".into()))?;
        let catalog = CellCatalog::new(self.layout.clone(), target.tenant());
        if catalog
            .lookup(target.cell_id())
            .await
            .map_err(|_| internal_error())?
            .is_none()
        {
            return Ok(None);
        }
        let output = self
            .client
            .query::<ReadCredential>(&target, None, Json(access_key_id.into()))
            .await
            .map_err(|_| internal_error())?;
        output
            .output
            .0
            .map(|record| self.decrypt(record))
            .transpose()
    }

    fn decrypt(
        &self,
        record: CredentialRecord,
    ) -> std::result::Result<StoredCredential, DynamoDbError> {
        let (nonce, encrypted) = record
            .encrypted_secret
            .split_first_chunk::<12>()
            .ok_or_else(internal_error)?;
        let cipher =
            Aes256Gcm::new_from_slice(&*self.encryption_key).map_err(|_| internal_error())?;
        let plaintext = cipher
            .decrypt(
                Nonce::from_slice(nonce),
                Payload {
                    msg: encrypted,
                    aad: record.access_key_id.as_bytes(),
                },
            )
            .map_err(|_| internal_error())?;
        let secret_key = String::from_utf8(plaintext).map_err(|_| internal_error())?;
        Ok(StoredCredential {
            secret_key,
            account_id: record.account_id,
            principal_name: record.principal_name,
            session_name: None,
            is_session: false,
            session_token: None,
            is_active: record.is_active,
            expires_at: None,
        })
    }
}

#[async_trait::async_trait]
impl CredentialStore for CellCredentialStore {
    async fn lookup_credential(
        &self,
        access_key_id: &str,
    ) -> std::result::Result<Option<StoredCredential>, DynamoDbError> {
        self.read(access_key_id).await
    }
}

fn internal_error() -> DynamoDbError {
    DynamoDbError::InternalServerError("credential lookup failed".into())
}
