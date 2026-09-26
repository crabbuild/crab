//! Cell-owned authorization catalog for ExtendDB's HTTP request gate.
//!
//! The pinned ExtendDB handler requires `CatalogStore` to be present even
//! though DynamoDB authorization reads `AuthorizationStore` separately. IAM
//! management, admin login, settings, and metrics are not implemented yet;
//! those operations return explicit errors. Login rate checks fail closed.

use crab_cell_runtime::client::CellClient;
use extenddb_storage::authorization_store::{AuthorizationStore, SessionData};
use extenddb_storage::management_store::{
    AccessKeyCreated, AccountDetail, AdminEntry, AdminStore, GroupDetail, GroupListEntry,
    ManagementStore, MetricsRow, MetricsStore, OpError, OpResult, RateLimitStore, RoleDetail,
    RoleListEntry, SettingsStore, UserDetail, UserListEntry,
};
use extenddb_storage::{BoxedFuture, CatalogStore};

use crate::CellAuthorizationStore;

/// Catalog backed by BeyondDB Cells for authorization decisions.
///
/// Management methods remain unavailable until their state and transaction
/// contracts are implemented in Cells. The server fails those calls explicitly.
pub struct CellCatalogStore {
    authorization: CellAuthorizationStore,
}

impl CellCatalogStore {
    /// Bind the account Cell client used for every authorization read.
    pub fn new(client: CellClient) -> Self {
        Self {
            authorization: CellAuthorizationStore::new(client),
        }
    }
}

macro_rules! unavailable {
    ($name:ident ( $($arg:ident : $ty:ty),* $(,)? ) -> $output:ty) => {
        fn $name(&self, $($arg: $ty),*) -> BoxedFuture<'_, OpResult<$output>> {
            let _ = ($($arg,)*);
            Box::pin(async {
                Err(OpError::Internal(concat!("BeyondDB management operation ", stringify!($name), " is unavailable").into()))
            })
        }
    };
}

impl SettingsStore for CellCatalogStore {
    unavailable!(get_setting(key: &str) -> Option<String>);
    unavailable!(set_setting(key: &str, value: &str) -> ());
    unavailable!(list_settings() -> Vec<(String, String)>);
}

impl MetricsStore for CellCatalogStore {
    unavailable!(insert_metrics(rows: &[MetricsRow]) -> ());
    unavailable!(query_metrics(start: time::OffsetDateTime, end: time::OffsetDateTime, table_name: Option<&str>, metric: Option<&str>) -> Vec<MetricsRow>);
    unavailable!(prune_metrics(retention: std::time::Duration) -> ());
}

impl RateLimitStore for CellCatalogStore {
    unavailable!(count_principal_failures(principal: &str, window_seconds: i64) -> i64);
    unavailable!(count_ip_failures(source_ip: &str, window_seconds: i64) -> i64);

    fn record_failed_login(
        &self,
        _principal: &str,
        _source_ip: Option<&str>,
    ) -> BoxedFuture<'_, ()> {
        // Login checks fail before authentication; these void hooks cannot
        // weaken that denial while the management surface is unavailable.
        Box::pin(async {})
    }

    fn cleanup_old_attempts(&self, _max_age_seconds: i64) -> BoxedFuture<'_, ()> {
        Box::pin(async {})
    }
}

impl AdminStore for CellCatalogStore {
    unavailable!(create_admin(admin_name: &str, password_hash: &str) -> ());
    unavailable!(list_admins() -> Vec<AdminEntry>);
    unavailable!(delete_admin(admin_name: &str) -> ());
    unavailable!(change_admin_password(admin_name: &str, password_hash: &str) -> ());
    unavailable!(verify_admin_password(admin_name: &str, password: &str) -> Option<bool>);
}

impl ManagementStore for CellCatalogStore {
    unavailable!(create_account(account_id: &str, account_name: &str) -> ());
    unavailable!(delete_account(account_id: &str) -> ());
    unavailable!(list_all_accounts() -> Vec<(String, String)>);
    unavailable!(default_account_id() -> Option<String>);
    unavailable!(list_all_accounts_full() -> Vec<(String, String, time::OffsetDateTime)>);
    unavailable!(list_accounts_for(account_id: &str) -> Vec<(String, String)>);
    unavailable!(get_account_detail(account_id: &str) -> Option<AccountDetail>);
    unavailable!(dashboard_counts() -> (i64, i64));
    unavailable!(create_user(account_id: &str, user_name: &str, password_hash: Option<&str>) -> ());
    unavailable!(delete_user(account_id: &str, user_name: &str) -> ());
    unavailable!(list_users(account_id: &str) -> Vec<UserListEntry>);
    unavailable!(get_user_detail(account_id: &str, user_name: &str) -> Option<UserDetail>);
    unavailable!(verify_iam_user_password(account_id: &str, user_name: &str, password: &str) -> bool);
    unavailable!(change_user_password(account_id: &str, user_name: &str, password_hash: &str) -> ());
    unavailable!(tag_user(account_id: &str, user_name: &str, tags: &[(String, String)]) -> ());
    unavailable!(untag_user(account_id: &str, user_name: &str, tag_keys: &[String]) -> ());
    unavailable!(list_user_tags(account_id: &str, user_name: &str) -> Vec<(String, String)>);
    unavailable!(create_group(account_id: &str, group_name: &str) -> ());
    unavailable!(delete_group(account_id: &str, group_name: &str) -> ());
    unavailable!(list_groups(account_id: &str) -> Vec<GroupListEntry>);
    unavailable!(get_group_detail(account_id: &str, group_name: &str) -> Option<GroupDetail>);
    unavailable!(add_group_member(account_id: &str, group_name: &str, user_name: &str) -> ());
    unavailable!(remove_group_member(account_id: &str, group_name: &str, user_name: &str) -> ());
    unavailable!(create_role(account_id: &str, role_name: &str, trust_policy: &serde_json::Value) -> ());
    unavailable!(delete_role(account_id: &str, role_name: &str) -> ());
    unavailable!(list_roles(account_id: &str) -> Vec<RoleListEntry>);
    unavailable!(get_role_detail(account_id: &str, role_name: &str) -> Option<RoleDetail>);
    unavailable!(get_role_trust_policy(account_id: &str, role_name: &str) -> Option<serde_json::Value>);
    unavailable!(tag_role(account_id: &str, role_name: &str, tags: &[(String, String)]) -> ());
    unavailable!(untag_role(account_id: &str, role_name: &str, tag_keys: &[String]) -> ());
    unavailable!(list_role_tags(account_id: &str, role_name: &str) -> Vec<(String, String)>);
    unavailable!(put_policy(account_id: &str, principal_type: &str, principal_name: &str, policy_name: &str, document: &serde_json::Value) -> ());
    unavailable!(delete_policy(account_id: &str, principal_type: &str, principal_name: &str, policy_name: &str) -> ());
    unavailable!(list_policies(account_id: &str, principal_type: &str, principal_name: &str) -> Vec<(String, serde_json::Value, time::OffsetDateTime)>);
    unavailable!(set_user_boundary(account_id: &str, user_name: &str, document: &serde_json::Value) -> ());
    unavailable!(get_user_boundary(account_id: &str, user_name: &str) -> Option<serde_json::Value>);
    unavailable!(delete_user_boundary(account_id: &str, user_name: &str) -> ());
    unavailable!(set_role_boundary(account_id: &str, role_name: &str, document: &serde_json::Value) -> ());
    unavailable!(get_role_boundary(account_id: &str, role_name: &str) -> Option<serde_json::Value>);
    unavailable!(delete_role_boundary(account_id: &str, role_name: &str) -> ());
    unavailable!(create_access_key(account_id: &str, user_name: &str) -> AccessKeyCreated);
    unavailable!(delete_access_key(account_id: &str, user_name: &str, key_id: &str) -> ());
    unavailable!(list_access_keys(account_id: &str, user_name: &str) -> Vec<(String, bool, time::OffsetDateTime)>);
    unavailable!(import_access_key(account_id: &str, user_name: &str, access_key_id: &str, secret_access_key: &str) -> ());
    unavailable!(store_session(session_token: &str, access_key_id: &str, secret_key_encrypted: &[u8], account_id: &str, role_name: &str, session_name: &str, session_tags: &Option<serde_json::Value>, session_policy: &Option<serde_json::Value>, expires_at: time::OffsetDateTime) -> ());
    unavailable!(fetch_caller_tags(account_id: &str, resource: &str) -> Vec<(String, String)>);
}

impl AuthorizationStore for CellCatalogStore {
    fn fetch_user_policies(
        &self,
        account_id: &str,
        user_name: &str,
    ) -> BoxedFuture<'_, OpResult<Vec<String>>> {
        self.authorization
            .fetch_user_policies(account_id, user_name)
    }

    fn fetch_user_group_policies(
        &self,
        account_id: &str,
        user_name: &str,
    ) -> BoxedFuture<'_, OpResult<Vec<String>>> {
        self.authorization
            .fetch_user_group_policies(account_id, user_name)
    }

    fn fetch_user_boundary(
        &self,
        account_id: &str,
        user_name: &str,
    ) -> BoxedFuture<'_, OpResult<Option<String>>> {
        self.authorization
            .fetch_user_boundary(account_id, user_name)
    }

    fn fetch_role_policies(
        &self,
        account_id: &str,
        role_name: &str,
    ) -> BoxedFuture<'_, OpResult<Vec<String>>> {
        self.authorization
            .fetch_role_policies(account_id, role_name)
    }

    fn fetch_role_boundary(
        &self,
        account_id: &str,
        role_name: &str,
    ) -> BoxedFuture<'_, OpResult<Option<String>>> {
        self.authorization
            .fetch_role_boundary(account_id, role_name)
    }

    fn fetch_session_data(
        &self,
        account_id: &str,
        role_name: &str,
        session_name: &str,
    ) -> BoxedFuture<'_, OpResult<Option<SessionData>>> {
        self.authorization
            .fetch_session_data(account_id, role_name, session_name)
    }

    fn fetch_user_tags(
        &self,
        account_id: &str,
        user_name: &str,
    ) -> BoxedFuture<'_, OpResult<Vec<(String, String)>>> {
        self.authorization.fetch_user_tags(account_id, user_name)
    }

    fn fetch_role_tags(
        &self,
        account_id: &str,
        role_name: &str,
    ) -> BoxedFuture<'_, OpResult<Vec<(String, String)>>> {
        self.authorization.fetch_role_tags(account_id, role_name)
    }

    fn fetch_resource_tags(&self, arn: &str) -> BoxedFuture<'_, OpResult<Vec<(String, String)>>> {
        self.authorization.fetch_resource_tags(arn)
    }
}

impl CatalogStore for CellCatalogStore {
    fn cached_encryption_key(&self) -> Option<String> {
        None
    }
}
