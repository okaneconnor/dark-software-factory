# terraform-azure

A dsf standards pack for Terraform root modules on Azure (`hashicorp/azurerm`
4.x). It's an ordinary Claude Code plugin, so it also works interactively:
`claude --plugin-dir examples/packs/terraform-azure`.

- `rules.md` is added to every agent's system prompt: the rules that always
  apply.
- `skills/terraform-azure/` is loaded when relevant: naming, variables and
  outputs, tests, Container Apps and Log Analytics, security, and a review
  checklist.
- `hooks/hooks.json` runs `terraform fmt` on every Terraform file an agent
  writes.

Pair it with the Terraform target-repo kit (`examples/target-repos/terraform`),
whose `verify.sh` checks what this pack asks for. Copy it and adapt the rules
to your own conventions.
