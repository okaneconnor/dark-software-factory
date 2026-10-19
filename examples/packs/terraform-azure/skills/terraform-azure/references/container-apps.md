# Container Apps and Log Analytics

```hcl
resource "azurerm_log_analytics_workspace" "main" {
  name                = local.log_analytics_workspace_name
  location            = azurerm_resource_group.main.location
  resource_group_name = azurerm_resource_group.main.name
  sku                 = "PerGB2018"
  retention_in_days   = var.log_retention_days # 30 to 730
  tags                = local.tags
}

resource "azurerm_container_app_environment" "main" {
  name                       = local.container_app_environment_name
  location                   = azurerm_resource_group.main.location
  resource_group_name        = azurerm_resource_group.main.name
  log_analytics_workspace_id = azurerm_log_analytics_workspace.main.id
  tags                       = local.tags
}

resource "azurerm_container_app" "main" {
  name                         = local.container_app_name
  container_app_environment_id = azurerm_container_app_environment.main.id
  resource_group_name          = azurerm_resource_group.main.name
  revision_mode                = "Single"
  tags                         = local.tags

  template {
    min_replicas = var.min_replicas
    max_replicas = var.max_replicas

    container {
      name   = "web"
      image  = var.container_image
      cpu    = 0.25
      memory = "0.5Gi"
    }
  }

  ingress {
    external_enabled = true
    target_port      = var.target_port

    traffic_weight {
      latest_revision = true
      percentage      = 100
    }
  }
}

output "url" {
  description = "HTTPS address of the web app."
  value       = "https://${azurerm_container_app.main.latest_revision_fqdn}"
}
```

- CPU and memory must be a supported pair, memory twice the CPU in Gi:
  0.25/0.5Gi, 0.5/1Gi, 0.75/1.5Gi, 1/2Gi and so on.
- `min_replicas = 0` lets the app scale to zero when idle.
- Ingress serves HTTPS; leave `allow_insecure_connections` at its default of
  `false`.
- To pull from a private registry, give the app a managed identity with
  `AcrPull`; never put a registry password in Terraform.
