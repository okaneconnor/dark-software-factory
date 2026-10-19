#!/bin/sh
# dsf's definition of done for a Terraform root module. It never plans or
# applies: dsf holds no cloud credentials, so tests use mock providers.
set -eu

if ! ls ./*.tf >/dev/null 2>&1; then
    echo "no Terraform files in the repository root" >&2
    exit 1
fi
terraform fmt -check -recursive -diff
terraform init -backend=false -input=false -no-color
terraform validate -no-color
if ls tests/*.tftest.hcl >/dev/null 2>&1; then
    terraform test -no-color
fi
