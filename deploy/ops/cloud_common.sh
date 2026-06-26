# Shared helpers for cloud ops scripts (source from deploy/ops/*.sh).
# Resolves EC2 Name tag (app_name) from env, .bittice_cloud.json, or terraform.tfvars.

resolve_default_app_name() {
  local repo_root="${1:?repo root required}"
  if [[ -n "${BITTICE_APP_NAME:-}" ]]; then
    echo "${BITTICE_APP_NAME}"
    return 0
  fi
  local cloud_json="${repo_root}/data/.bittice_cloud.json"
  if [[ -f "${cloud_json}" ]]; then
    local from_json
    from_json="$(python3 - "${cloud_json}" <<'PY' 2>/dev/null || true
import json, sys
print(json.load(open(sys.argv[1])).get("app_name") or "")
PY
)"
    if [[ -n "${from_json}" ]]; then
      echo "${from_json}"
      return 0
    fi
  fi
  local tfvars="${repo_root}/data/terraform/terraform.tfvars"
  if [[ -f "${tfvars}" ]]; then
    local from_tf
    from_tf="$(grep -E '^[[:space:]]*app_name[[:space:]]*=' "${tfvars}" \
      | sed -E 's/^[[:space:]]*app_name[[:space:]]*=[[:space:]]*"([^"]+)".*/\1/' \
      | head -1)"
    if [[ -n "${from_tf}" ]]; then
      echo "${from_tf}"
      return 0
    fi
  fi
  echo "dash-sac-dev"
}
