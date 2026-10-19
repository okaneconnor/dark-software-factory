These apply to every Terraform change in this repository.

- **Layout.** One root module, files by concern: `versions.tf`, `variables.tf`, `locals.tf`, `outputs.tf`, `main.tf`, and one file per area such as `monitoring.tf`. No resources in `versions.tf`, `variables.tf` or `outputs.tf`.
- **Versions.** Pin `required_version` and every provider in `versions.tf` with `~>`. Keep `.terraform.lock.hcl` committed when it changes.
- **Names.** Build every resource name once, in `locals.tf`, from `var.name` and `var.environment`, starting with the resource's Cloud Adoption Framework abbreviation (`rg-`, `log-`, `cae-`, `ca-`, `kv-`; `st` with no hyphens for storage). Never write a name inline in a resource.
- **Tags.** Every taggable resource gets `tags = local.tags`.
- **Variables.** Every variable has a `type` and a `description`, a `default` only when a safe one exists, and a `validation` block when it has a known valid range or format. Mark secrets `sensitive = true`.
- **Outputs.** Every output has a `description`; secrets are `sensitive = true`.
- **Tests.** Every change adds or updates `tests/*.tftest.hcl` using `mock_provider "azurerm" {}`, asserting the values the change is about, with `expect_failures` for each validation.
- **Never** add credentials, subscription or tenant IDs, a `backend` block, provisioners, or `local-exec`; never run `terraform plan` or `apply`.

Load the `terraform-azure:terraform-azure` skill for the naming table, test patterns, resource guidance and the review checklist.
