mock_provider "azurerm" {}

override_data {
  target = data.azurerm_resource_group.server
  values = {
    name     = "test-resource-group"
    location = "eastus"
  }
}

override_resource {
  target          = azurerm_storage_container.repositories
  override_during = plan
  values = {
    id = "/subscriptions/test/resourceGroups/test/providers/Microsoft.Storage/storageAccounts/testcrabhttpserver/blobServices/default/containers/crab-repositories"
  }
}

override_resource {
  target          = azurerm_user_assigned_identity.server
  override_during = plan
  values = {
    id           = "/subscriptions/test/resourceGroups/test/providers/Microsoft.ManagedIdentity/userAssignedIdentities/crab-http-server"
    client_id    = "11111111-1111-1111-1111-111111111111"
    principal_id = "22222222-2222-2222-2222-222222222222"
  }
}

variables {
  resource_group_name  = "test-resource-group"
  storage_account_name = "testcrabhttpserver"
  aks_oidc_issuer_url  = "https://oidc.prod-aks.azure.com/example/cluster/"
}

run "hardens_the_storage_boundary" {
  command = plan

  assert {
    condition = (
      azurerm_storage_account.repositories.allow_nested_items_to_be_public == false &&
      azurerm_storage_account.repositories.default_to_oauth_authentication == true &&
      azurerm_storage_account.repositories.https_traffic_only_enabled == true &&
      azurerm_storage_account.repositories.infrastructure_encryption_enabled == true &&
      azurerm_storage_account.repositories.min_tls_version == "TLS1_2" &&
      azurerm_storage_account.repositories.public_network_access == "Enabled" &&
      azurerm_storage_account.repositories.shared_access_key_enabled == false
    )
    error_message = "Azure Storage must require OAuth and TLS while disabling public blobs and shared keys."
  }

  assert {
    condition     = one(azurerm_storage_account.repositories.blob_properties).versioning_enabled == true
    error_message = "Azure Storage object versioning must remain enabled."
  }

  assert {
    condition     = azurerm_storage_container.repositories.container_access_type == "private"
    error_message = "The Crab Blob container must remain private."
  }
}

run "disables_public_network_access_for_private_endpoints" {
  command = plan

  variables {
    public_network_access_enabled = false
  }

  assert {
    condition     = azurerm_storage_account.repositories.public_network_access == "Disabled"
    error_message = "The private-endpoint profile must disable Azure Storage public network access."
  }
}

run "scopes_workload_identity_to_the_server" {
  command = plan

  assert {
    condition = (
      toset(azurerm_federated_identity_credential.server.audience) == toset(["api://AzureADTokenExchange"]) &&
      azurerm_federated_identity_credential.server.issuer == var.aks_oidc_issuer_url &&
      azurerm_federated_identity_credential.server.subject == "system:serviceaccount:${var.namespace}:${var.service_account_name}"
    )
    error_message = "AKS federation must bind the expected issuer, audience, namespace, and ServiceAccount."
  }

  assert {
    condition = (
      azurerm_role_assignment.storage.scope == azurerm_storage_container.repositories.id &&
      azurerm_role_assignment.storage.role_definition_name == "Storage Blob Data Contributor" &&
      azurerm_role_assignment.storage.principal_id == azurerm_user_assigned_identity.server.principal_id
    )
    error_message = "The managed identity must receive data-plane access only at the dedicated container."
  }
}

run "expires_only_short_lived_auth_state" {
  command = plan

  assert {
    condition = toset(one([
      for rule in azurerm_storage_management_policy.repositories.rule : rule
      if rule.name == "expireCrabAuthState"
    ]).filters[0].prefix_match) == toset(["${var.container_name}/${var.root_prefix}/.crab/http-server/v1/auth/"])
    error_message = "The one-day lifecycle rule must remain scoped to Crab's short-lived auth namespace."
  }

  assert {
    condition = (
      one(one([
        for rule in azurerm_storage_management_policy.repositories.rule : rule
        if rule.name == "expireCrabAuthState"
      ]).actions).base_blob[0].delete_after_days_since_modification_greater_than == 1 &&
      one(one([
        for rule in azurerm_storage_management_policy.repositories.rule : rule
        if rule.name == "expireCrabAuthState"
      ]).actions).version[0].delete_after_days_since_creation == 1
    )
    error_message = "Azure auth objects and their versions must expire after one day."
  }
}

run "rejects_an_unknown_replication_type" {
  command = plan

  variables {
    account_replication_type = "RAGRS"
  }

  expect_failures = [var.account_replication_type]
}
