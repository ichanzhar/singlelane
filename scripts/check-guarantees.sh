#!/usr/bin/env bash
# Fails if a guarantee listed in scripts/guarantees.txt has no test whose name starts with
# its id (SPEC section 2: `g2_container_env_has_no_github_token` for G2).
set -euo pipefail
cd "$(dirname "$0")/.."

status=0
while read -r id _; do
  [[ -z "$id" || "$id" == \#* ]] && continue
  n="${id#G}"
  if [[ ! "$n" =~ ^[0-9]+$ ]]; then
    echo "FAIL $id: not a guarantee id"
    status=1
    continue
  fi
  count=$( (grep -rhoE "fn g${n}_[a-z0-9_]+" crates --include='*.rs' || true) | sort -u | wc -l)
  if (( count == 0 )); then
    echo "FAIL $id: no test whose name starts with g${n}_"
    status=1
  else
    echo "PASS $id: $count test(s)"
  fi
done < scripts/guarantees.txt
exit "$status"
