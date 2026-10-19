# Variables and outputs

```hcl
variable "name" {
  type        = string
  description = "Base name of the app, used in every resource name. 3 to 20 lowercase letters and digits."

  validation {
    condition     = can(regex("^[a-z0-9]{3,20}$", var.name))
    error_message = "name must be 3 to 20 lowercase letters and digits."
  }
}

variable "environment" {
  type        = string
  description = "Deployment environment: dev, test or prod."
  default     = "dev"

  validation {
    condition     = contains(["dev", "test", "prod"], var.environment)
    error_message = "environment must be one of dev, test or prod."
  }
}
```

- Ranges: `condition = var.max_replicas >= 0 && var.max_replicas <= 10`.
- Since Terraform 1.9 a validation may refer to other variables, so check pairs
  in one place:

  ```hcl
  validation {
    condition     = var.min_replicas <= var.max_replicas
    error_message = "min_replicas must not exceed max_replicas."
  }
  ```

- Use `nullable = false` when `null` would break the module.
- Prefer `object({ ... })` with `optional()` attributes over many loose
  variables for one setting group.
- Secrets: `sensitive = true`, no `default`.

Every output has a `description`. Output the values a person needs after
`apply` (names, URLs, IDs), and mark secrets `sensitive = true`.
