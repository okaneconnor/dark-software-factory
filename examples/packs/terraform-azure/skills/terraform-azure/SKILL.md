---
name: terraform-azure
description: Conventions and patterns for Terraform on Azure with azurerm 4.x. Covers Cloud Adoption Framework resource naming and length limits, tags, variable validation, outputs, mock-provider tests with terraform test, Container Apps and Log Analytics, and security defaults, plus a review checklist. Use before writing, changing or reviewing any Terraform in this repository.
---

# Terraform on Azure

The reference pages are in `references/`, next to this file. Read the ones the
task needs before writing code:

| Task | Read |
| --- | --- |
| Naming a resource | `references/naming.md` |
| Variables, validation, outputs | `references/variables-and-outputs.md` |
| Writing or fixing tests | `references/testing.md` |
| Container Apps, Log Analytics | `references/container-apps.md` |
| Identity, secrets, networking, storage, Key Vault | `references/security.md` |
| Reviewing a change | all of them, then the checklist below |

## Review checklist

A change is ready when:

- [ ] Every new resource's name comes from `locals.tf` and follows `references/naming.md`, within the resource's length limit.
- [ ] Every taggable resource has `tags = local.tags`.
- [ ] Every new variable has a `type`, a `description` and, where input can be wrong, a `validation` with a clear `error_message`.
- [ ] Every new output has a `description`, and secrets are `sensitive`.
- [ ] Tests assert the values the change is about, and every validation has an `expect_failures` run.
- [ ] Providers are pinned with `~>`, and there are no credentials, IDs, backends or provisioners.
- [ ] The security defaults in `references/security.md` hold for anything it touches.
