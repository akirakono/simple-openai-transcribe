#!/usr/bin/env bash
set -euo pipefail

profile="release"
enable_now="true"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --debug)
      profile="debug"
      shift
      ;;
    --release)
      profile="release"
      shift
      ;;
    --no-enable)
      enable_now="false"
      shift
      ;;
    -h|--help)
      cat <<'EOF'
Usage: scripts/install-systemd-user-service.sh [--debug|--release] [--no-enable]

Installs the user service into ~/.config/systemd/user using the current
repository path and the selected cargo profile's binary path.
EOF
      exit 0
      ;;
    *)
      echo "Unknown argument: $1" >&2
      exit 1
      ;;
  esac
done

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
template_path="${repo_root}/systemd/simple-openai-transcribe.service.in"
binary_path="${repo_root}/target/${profile}/simple-openai-transcribe"
service_dir="${HOME}/.config/systemd/user"
service_path="${service_dir}/simple-openai-transcribe.service"

if [[ ! -x "${binary_path}" ]]; then
  echo "Binary not found: ${binary_path}" >&2
  echo "Build it first with: cargo build --${profile}" >&2
  exit 1
fi

mkdir -p "${service_dir}"

escaped_repo_root="$(printf '%s' "${repo_root}" | sed 's/[&|]/\\&/g')"
escaped_binary_path="$(printf '%s' "${binary_path}" | sed 's/[&|]/\\&/g')"

sed \
  -e "s|@WORKDIR@|${escaped_repo_root}|g" \
  -e "s|@EXECSTART@|${escaped_binary_path}|g" \
  "${template_path}" > "${service_path}"

systemctl --user daemon-reload

if [[ "${enable_now}" == "true" ]]; then
  systemctl --user enable --now simple-openai-transcribe.service
else
  systemctl --user enable simple-openai-transcribe.service
fi

echo "Installed ${service_path}"
echo "Binary: ${binary_path}"
