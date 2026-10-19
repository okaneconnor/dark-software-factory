# Naming

Names are built once, in `locals.tf`:

```hcl
locals {
  resource_group_name = "rg-${var.name}-${var.environment}"
  # Storage accounts allow only lowercase letters and digits.
  storage_account_name = "st${var.name}${var.environment}"
}
```

The pattern is `<abbreviation>-<name>-<environment>`. Add a region or an
instance number at the end only when the issue needs several of the same
thing.

| Resource | Abbreviation | Notes |
| --- | --- | --- |
| Resource group | `rg` | up to 90 characters |
| Log Analytics workspace | `log` | 4–63 characters |
| Application Insights | `appi` | |
| Container Apps environment | `cae` | |
| Container app | `ca` | up to 32 characters: lowercase letters, digits and hyphens, starting with a letter |
| Container registry | `cr` | letters and digits only, globally unique: `cr<name><environment>` |
| Key Vault | `kv` | 3–24 characters, globally unique |
| Storage account | `st` | 3–24 lowercase letters and digits, globally unique: `st<name><environment>` |
| User-assigned managed identity | `id` | |
| Virtual network | `vnet` | |
| Subnet | `snet` | |
| Network security group | `nsg` | |
| Public IP address | `pip` | |
| App Service plan | `asp` | |
| Web app | `app` | |
| Function app | `func` | |
| Azure SQL server | `sql` | globally unique |
| Azure SQL database | `sqldb` | |
| PostgreSQL flexible server | `psql` | globally unique |
| Service Bus namespace | `sbns` | globally unique |

When `var.name` is validated as 3–20 lowercase letters and digits and
`var.environment` is `dev`, `test` or `prod`, every name above fits its limit.
Check the limit whenever you add a resource or loosen those validations.
