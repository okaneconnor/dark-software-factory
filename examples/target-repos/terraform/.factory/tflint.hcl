# Lint rules for verify. In .factory/, so an agent can't loosen them: dsf
# protects this directory and always runs the base commit's copy.
config {
  call_module_type = "local"
}

# Built into tflint. "recommended" covers required versions and providers,
# typed variables and unused declarations.
plugin "terraform" {
  enabled = true
  preset  = "recommended"
}

# Installed in dsf's worker image: invalid SKUs, sizes and similar azurerm
# mistakes.
plugin "azurerm" {
  enabled = true
}

rule "terraform_documented_variables" {
  enabled = true
}

rule "terraform_documented_outputs" {
  enabled = true
}

rule "terraform_naming_convention" {
  enabled = true
}

# main.tf, variables.tf and outputs.tf exist, and variables and outputs
# live only in their own files.
rule "terraform_standard_module_structure" {
  enabled = true
}
