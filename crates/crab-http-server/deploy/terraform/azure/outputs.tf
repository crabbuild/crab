output "storage_url" {
  description = "Crab storage.url value."
  value       = "az://${azurerm_storage_account.repositories.name}/${azurerm_storage_container.repositories.name}/${var.root_prefix}"
}

output "storage_account_name" {
  description = "Dedicated Azure Storage account name."
  value       = azurerm_storage_account.repositories.name
}

output "container_name" {
  description = "Dedicated private Blob container name."
  value       = azurerm_storage_container.repositories.name
}

output "managed_identity_client_id" {
  description = "Value for the Helm AKS ServiceAccount annotation."
  value       = azurerm_user_assigned_identity.server.client_id
}

output "service_account_name" {
  description = "Helm serviceAccount.name value."
  value       = var.service_account_name
}
