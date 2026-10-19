# Testing with `terraform test`

Tests live in `tests/*.tftest.hcl` and run against a mock provider, so they
need no credentials. One file per area (`tests/naming.tftest.hcl`,
`tests/container_apps.tftest.hcl`) keeps them readable.

```hcl
mock_provider "azurerm" {}

variables {
  name = "webapp"
}

run "names_follow_the_convention" {
  command = plan

  assert {
    condition     = azurerm_resource_group.main.name == "rg-webapp-dev"
    error_message = "Resource group should be rg-webapp-dev."
  }
}

run "bad_names_are_rejected" {
  command = plan

  variables {
    name = "Web_App"
  }

  expect_failures = [var.name]
}
```

- Use `command = plan` for anything known from inputs: names, tags, settings.
- Computed attributes (IDs, FQDNs) are unknown at plan. Use `command = apply`
  (the mock invents values) and give the mock the value you want to assert:

  ```hcl
  mock_provider "azurerm" {
    mock_resource "azurerm_container_app" {
      defaults = {
        latest_revision_fqdn = "ca-webapp-dev.example.azurecontainerapps.io"
      }
    }
  }
  ```

- With `command = apply`, the mock invents random strings, and some
  resources check their inputs' format at apply. Give the mocks well-formed
  values wherever that bites, or the run fails with a parse error:
  - any resource whose `id` another resource takes, such as the Log Analytics
    workspace a Container Apps environment points at:

    ```hcl
    mock_resource "azurerm_log_analytics_workspace" {
      defaults = {
        id = "/subscriptions/00000000-0000-0000-0000-000000000000/resourceGroups/rg-webapp-dev/providers/Microsoft.OperationalInsights/workspaces/log-webapp-dev"
      }
    }
    mock_resource "azurerm_container_app_environment" {
      defaults = {
        id = "/subscriptions/00000000-0000-0000-0000-000000000000/resourceGroups/rg-webapp-dev/providers/Microsoft.App/managedEnvironments/cae-webapp-dev"
      }
    }
    ```

  - UUIDs from data sources, such as `tenant_id` for a Key Vault:

    ```hcl
    mock_data "azurerm_client_config" {
      defaults = {
        tenant_id = "00000000-0000-0000-0000-000000000000"
      }
    }
    ```

  These blocks go inside `mock_provider "azurerm" { ... }`. Prefer
  `command = plan` where it can answer the question: it needs none of this.
- Every `validation` block gets a run with `expect_failures` naming the
  variable.
- Assert the values the issue asks for, not just that a resource exists.
- Never delete or weaken a test to make a change pass.
