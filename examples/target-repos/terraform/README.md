# Terraform target repo kit

Copy these into a Terraform root module's repository to let dsf work on it:

- `.factory/verify.sh`, the definition of done: `terraform fmt`, `init`
  without a backend, `validate`, and `terraform test` when there are tests.
  It never plans or applies. dsf never holds cloud credentials, so tests
  use `mock_provider`.
- `.factory/workflow.toml`, the default workflow except that verify may use
  the egress proxy, because `terraform init` downloads providers.
- `.gitignore`, which keeps `.terraform/` and state files out of commits.
  Without it the agent's working copy would commit the provider binaries.
- `CLAUDE.md`, conventions for the agent. Adapt it to the repo.

On the dsf host, allow the Terraform registry in the config:

```toml
[egress]
allow = ["registry.terraform.io", "releases.hashicorp.com"]
```

The worker image includes Terraform, pinned in `worker/Dockerfile`.

For conventions beyond what verify checks, give the repo the
`terraform-azure` standards pack (`examples/packs/terraform-azure`), or your
own, in the config's `packs`.
