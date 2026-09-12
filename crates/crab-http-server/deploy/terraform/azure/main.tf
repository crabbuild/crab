data "azurerm_resource_group" "server" {
  name = var.resource_group_name
}

locals {
  auth_prefix = "${var.root_prefix}/.crab/http-server/v1/auth/"
}

resource "azurerm_storage_account" "repositories" {
  name                              = var.storage_account_name
  resource_group_name               = data.azurerm_resource_group.server.name
  location                          = data.azurerm_resource_group.server.location
  account_tier                      = "Standard"
  account_kind                      = "StorageV2"
  account_replication_type          = var.account_replication_type
  allow_nested_items_to_be_public   = false
  default_to_oauth_authentication   = true
  https_traffic_only_enabled        = true
  infrastructure_encryption_enabled = true
  min_tls_version                   = "TLS1_2"
  public_network_access_enabled     = var.public_network_access_enabled
  shared_access_key_enabled         = false
  tags                              = var.tags

  blob_properties {
    versioning_enabled = true

    container_delete_retention_policy {
      days = 7
    }

    delete_retention_policy {
      days = 7
    }
  }

  lifecycle {
    prevent_destroy = true
  }
}

resource "azurerm_storage_container" "repositories" {
  name                  = var.container_name
  storage_account_id    = azurerm_storage_account.repositories.id
  container_access_type = "private"
}

resource "azurerm_storage_management_policy" "repositories" {
  storage_account_id = azurerm_storage_account.repositories.id

  rule {
    name    = "expireCrabAuthState"
    enabled = true

    filters {
      prefix_match = ["${var.container_name}/${local.auth_prefix}"]
      blob_types   = ["blockBlob"]
    }

    actions {
      base_blob {
        delete_after_days_since_modification_greater_than = 1
      }

      version {
        delete_after_days_since_creation = 1
      }
    }
  }
}

resource "azurerm_user_assigned_identity" "server" {
  name                = var.managed_identity_name
  resource_group_name = data.azurerm_resource_group.server.name
  location            = data.azurerm_resource_group.server.location
  tags                = var.tags
}

resource "azurerm_federated_identity_credential" "server" {
  name                      = "crab-http-server"
  user_assigned_identity_id = azurerm_user_assigned_identity.server.id
  audience                  = ["api://AzureADTokenExchange"]
  issuer                    = var.aks_oidc_issuer_url
  subject                   = "system:serviceaccount:${var.namespace}:${var.service_account_name}"
}

resource "azurerm_role_assignment" "storage" {
  scope                = azurerm_storage_container.repositories.id
  role_definition_name = "Storage Blob Data Contributor"
  principal_id         = azurerm_user_assigned_identity.server.principal_id
  principal_type       = "ServicePrincipal"
}
