# Run the managed API

Start with one container and one persistent volume. Docker Compose runs the
same native API used by the Kubernetes deployment. No Cloudflare account, VM
tool runtime, or cluster is required.

This is currently a private, single-operator service. Account access controls
and computer pairing are [planned](../docs/MANAGED_SERVICE.md).

## Start

Install Docker with Compose support for secrets sourced from environment
variables. Supply the API token and existing model credential from your secret
store in the shell environment. Do not put their values in command arguments
or commit them to Git.

```sh
: "${NANOCODEX_API_TOKEN:?Set a separate service token with at least 32 characters}"
: "${OPENAI_API_KEY:?Load the existing model credential}"
docker compose up -d --build --wait
curl --fail http://127.0.0.1:8080/readyz
```

The API is at `http://127.0.0.1:8080/v1`. Use `NANOCODEX_API_TOKEN` as the SDK
API key. See [the API guide](../services/api/README.md) for saved agents, tools,
memory, and the exact SDK support boundary. Set `NANOCODEX_PORT` before startup
to change the local port. `OPENAI_BASE_URL` can select a compatible endpoint.

Compose reads the two environment variables and mounts secret files. The API
loads those files at startup. Its container environment contains file paths,
not the credential values. Host and container operators can still read them;
this is not confidential computing. Rotate a secret by replacing its source and
recreating the API container.

The container runs as UID 10001 with a temporary `/tmp` and a writable named
volume at `/data`. Compose needs a writable container layer to install secrets
sourced from environment variables. A new data volume gets the correct
ownership from the image. The volume holds the SQLite database. Keep one API
replica. Do not mount this database into a second active API container.

## Run on GCP or another server

Use the same Compose file on a Linux server with Docker. On GCP, one Compute
Engine VM with persistent disk storage is enough for the private pilot.
Build the image for the server's CPU architecture, or supply a matching image
with `NANOCODEX_IMAGE=registry/image@sha256:...` and use `up --no-build`.

The listener is bound to loopback. Connect through SSH forwarding or a private
network endpoint. Before a customer launch, implement account authorization
and add HTTPS. A public port is not a substitute for account isolation.
No GCP resources are created by this repository's Compose file.

The [GKE files](gcp/README.md) remain available for deployments that need a
cluster. They use the same image. There is no separate Kubernetes agent runtime.

## Update and retain data

```sh
docker compose up -d --build --wait
docker compose logs --tail=50 api
```

A container replacement causes brief downtime. Completed context and agent
memory survive through the named volume. Unfinished turns fail on restart;
durable turn recovery is a separate planned integration. Keep the Compose
project name fixed so that updates use the same volume.

`docker compose down` stops the service and retains the named volume.
`docker compose down --volumes` deletes its data; do not use it for an upgrade.
Back up the database before schema changes. For a simple consistent backup,
stop the API, copy the complete data volume, then start the API again. Test
restoration separately. Roll back only to an image with a compatible schema.

Sources: [Compose secrets](https://docs.docker.com/compose/how-tos/use-secrets/),
[Docker volumes](https://docs.docker.com/engine/storage/volumes/).
