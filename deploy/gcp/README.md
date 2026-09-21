# GCP pilot deployment

Status: deployment inputs are prepared; no GCP resources have been created.
The existing local kind deployment is unchanged.

Use GKE Autopilot for the first text-only pilot. The runtime needs no VM or
privileged container. Keep one API replica because this version uses SQLite.
The GKE overlay selects Linux amd64, requests 0.5 CPU and 1 GiB RAM, and gives
SQLite a 20 GiB `standard-rwo` volume. The storage is zonal; this is not a
high-availability deployment. The Service remains internal.

Required deployment inputs:

- Explicit GCP project ID and region, with billing enabled.
- Existing GKE cluster, or authorization to create a dedicated cluster.
- Existing model credential location and a separate API bearer token.
- Artifact Registry image digest from a successful Linux amd64 build.
- For iMessage later: provider/line credentials and a public HTTPS webhook URL.

## Build

Create an Artifact Registry Docker repository named `nanocodex` in the selected
region if it does not exist. The Cloud Build service account needs permission to
write that repository. GKE nodes need permission to read it. Scope each grant to
the required resource. Use an explicit project for every command.

```sh
: "${GCP_PROJECT_ID:?Set the approved project ID}"
: "${GCP_REGION:?Set the approved region}"
: "${IMAGE_TAG:?Set a source commit tag}"
gcloud builds submit . \
  --project="$GCP_PROJECT_ID" \
  --config=deploy/gcp/cloudbuild.yaml \
  --substitutions="_REGION=$GCP_REGION,_TAG=$IMAGE_TAG"
```

This uploads the source and incurs Cloud Build charges. Do not run it until the
project is selected. The local kind image was built for its local node; the GCP
build explicitly targets Linux amd64.

## Render and deploy

Use a separate kubeconfig file for the approved GCP cluster. Do not change the
user's default context. Set `KUBECONFIG` to that file and pass `--context` on each
command. Create namespace `nanocodex` and its `nanocodex-api` Kubernetes Secret
with `api-token` and `openai-api-key`, using the approved credential source.
Never put values in manifests, command arguments, source control, or logs.

Render with an immutable Artifact Registry image reference:

```sh
deploy/gcp/render.sh "$IMAGE_DIGEST_REFERENCE" > /tmp/nanocodex-gke.yaml
```

The renderer rejects mutable tags and does not contact GCP. It pins the image
without changing the local kind manifests. Review the rendered manifest before
applying it. The checked-in GKE overlay inherits the base image name; use this
renderer for the cloud deployment.

Before applying, verify the target context, image digest, credential references,
service type, resource requests, and storage. Run a server-side dry run on the
approved GKE context, then apply the same manifest. Verify rollout, readiness,
a real model turn, and memory after a pod replacement through an authenticated
port forward. Record the running image ID and test result.

GKE's Secret Manager add-on can replace Kubernetes Secret storage in a later
change. It mounts files; this API currently reads credentials from environment
variables. Do not assume the add-on automatically creates Kubernetes Secrets.

## iMessage and public access

The current API has no iMessage webhook. Do not make its operator API public as
a substitute. Add a channel endpoint with signature verification, replay checks,
durable receipt, verified sender-to-account binding, and a retry-safe reply
queue. Expose only that endpoint through HTTPS. Keep the operator API internal.

For a private pilot, admit only explicit paired senders. For customers, add
account authorization before admitting multiple users. Never let a supplied
`agent_id`, phone number, or group chat select another user's memory. A phone
number is a channel identity, not a permanent account identity.

## Rollout and rollback

The cloud pilot is separate from kind. It does not migrate local records or
interrupt the local service. A single-replica rollout can briefly stop service;
iMessage receipt and replies need durable queues before public use. Back up the
cloud database before schema changes. Roll back with the prior immutable image
only when its schema is compatible. Retain the PVC on rollback. Deleting a
cluster or volume is a separate action.

Sources: [GKE Autopilot](https://docs.cloud.google.com/kubernetes-engine/docs/concepts/autopilot-overview),
[Secret Manager add-on](https://docs.cloud.google.com/secret-manager/docs/secret-manager-managed-csi-component).
