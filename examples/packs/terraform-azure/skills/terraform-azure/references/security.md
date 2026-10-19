# Security defaults

- **No secrets in code.** Not in `.tf`, `.tfvars` or tests. Secrets come from
  Key Vault or the environment at apply time, and variables holding them are
  `sensitive = true`.
- **Identity over keys.** Use managed identities and role assignments; scope
  each role assignment to the narrowest resource that works.
- **HTTPS only.** Web ingress serves HTTPS; never allow insecure connections.
- **Storage accounts:** `min_tls_version = "TLS1_2"`,
  `allow_nested_items_to_be_public = false`, and
  `public_network_access_enabled = false` unless the issue needs public access.
- **Key Vault:** `rbac_authorization_enabled = true`,
  `purge_protection_enabled = true`, soft delete on.
- **Diagnostics.** Send resource logs to the Log Analytics workspace with
  `azurerm_monitor_diagnostic_setting` when the issue asks for monitoring.
- **No provisioners, no `local-exec`.** They run code at apply time that no
  review sees.
