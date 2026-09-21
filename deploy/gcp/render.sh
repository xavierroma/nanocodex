#!/usr/bin/env bash
set -euo pipefail
if [[ $# != 1 || ! "$1" =~ ^[a-z0-9-]+-docker\.pkg\.dev/[a-z0-9._/-]+@sha256:[a-f0-9]{64}$ ]]; then
  echo 'Usage: deploy/gcp/render.sh REGION-docker.pkg.dev/PROJECT/REPO/IMAGE@sha256:DIGEST' >&2
  exit 2
fi
repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
render_dir=$(mktemp -d "${TMPDIR:-/tmp}/nanocodex-gke.XXXXXX")
trap 'rm -rf -- "$render_dir"' EXIT
cp -R "$repo_root/deploy/k8s/base" "$repo_root/deploy/k8s/gke" "$render_dir/"
cat > "$render_dir/kustomization.yaml" <<MANIFEST
apiVersion: kustomize.config.k8s.io/v1beta1
kind: Kustomization
resources:
  - gke
images:
  - name: nanocodex-api
    newName: ${1%@*}
    digest: ${1#*@}
MANIFEST
kubectl kustomize "$render_dir"
