# Conventions for agents working on this repo

This is a Terraform root module. Changes arrive as draft pull requests,
and a person reviews and applies them.

- Never run `terraform plan` or `apply`, and never add credentials, backend
  configuration with secrets, or provider blocks that read them. There are
  no cloud credentials here.
- Run `terraform fmt -recursive` before finishing; `.factory/verify.sh`
  checks formatting, `validate` and `terraform test`.
- Every change comes with tests in `tests/*.tftest.hcl` that use
  `mock_provider`, so they run without credentials. Assert on the values
  that matter: names, tags, settings that were asked for.
- Every variable has a `description` and a `type`, and a `validation` block
  where bad input is likely. Every output has a `description`.
- Keep resources in files by concern (e.g. `network.tf`, `monitoring.tf`),
  and pin providers with `~>` constraints in `versions.tf`.
- Keep `.terraform.lock.hcl` committed when it changes.
