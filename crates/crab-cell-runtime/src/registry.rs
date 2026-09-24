//! Compiled primitive registry: descriptors, schemas, handlers, and builder.
mod descriptor;
pub use builder::{Registry, RegistryBuilder};
pub use handlers::{
    CellModule, Command, CommandContext, CommandInvocation, CommandResult, Query, QueryContext,
    QueryInvocation,
};
pub use schemas::{
    BuildDescriptor, MigrationDescriptor, MigrationPlan, ModuleDescriptor, NamespaceDescriptor,
    OperationDescriptor, RegistryError, RetainedCodeDescriptor,
};
mod builder;
mod handlers;
mod schemas;

use descriptor::{encode_release, requires_persisted_work_inventory, verify_rolling_compatibility};

use crate::cell::catalog::CatalogRole;
use crate::cell::executor::{HandlerOutcome, MutationIdentity};
use crate::client::{CellClient, Committed, InvocationError};
use crate::codec::WireValue;
use crate::codec::{decode_wire, encode_wire};
use crate::identity::{ApplicationId, CellId, CellTarget, Digest, NamespaceId, TenantId};
use crate::peer::EffectPeerClient;
use crate::primitives::activity_pool::BlockingActivityReservation;
use crate::primitives::effects::EffectBatch;
use crate::primitives::effects::{
    EffectCommandIntent, EffectModule, EffectRunOutcome, EffectSupervisor, EffectSupervisorError,
};
use crate::primitives::maintenance::{
    MaintenanceModule, MaintenanceTickCommand, MaintenanceTickOutcome, MaintenanceTickRequest,
};
use crate::primitives::sql::{SqlBatch, SqlResultSet, sql_batch, sql_query_batch};
use crate::primitives::workflow::{
    ActivityContext, ActivityExecution, ActivityHandler, ActivityRunOutcome, ActivitySupervisor,
    ActivitySupervisorError, ActivitySupport, BlockingActivityHandler, WorkflowActivities,
    WorkflowActivityModule, WorkflowDefinition,
};
use crate::{Error, Result};
