# NanoCodex Agents API

This service puts an HTTP API in front of the NanoCodex Rust engine. It runs in a
normal Kubernetes Pod. A VM, AX, and a confidential execution environment are not
required. The API is open source; model inference still uses an external provider.

The supported routes work with `openai@7.15.0` and
`client.beta.agents.sessions`. They implement a limited part of the
[OpenAI Agents API](https://developers.openai.com/api/docs/guides/agents-api/overview).
This is not a proxy to OpenAI's hosted Agents API. NanoCodex owns the agent loop
and calls the OpenAI Responses API for inference.

## Use with the OpenAI SDK

```ts
import OpenAI from 'openai';

const apiKey = process.env.NANOCODEX_API_TOKEN;
if (!apiKey) throw new Error('Set NANOCODEX_API_TOKEN');
const client = new OpenAI({
  baseURL: 'http://127.0.0.1:18080/v1',
  apiKey,
});
const session = await client.beta.agents.sessions.create({
  environment: { type: 'none' },
  agent: { model: 'gpt-5.6-sol', instructions: 'Give concise answers.' },
});
for await (const event of client.beta.agents.sessions.stream(session.id, {
  input: 'Explain how this API works.',
})) {
  if (event.type === 'agent.session.turn.output_text.delta') {
    process.stdout.write(event.delta);
  }
}
```

`NANOCODEX_API_TOKEN` authenticates clients to this service. `OPENAI_API_KEY` is the
separate provider credential, held by the service. Do not give the provider key
to API clients.

## Contract

| Route under `/v1` | Support |
| --- | --- |
| `POST /agents/sessions` | Inline `agent.model`, optional `agent.instructions`, `environment: {type: "none"}`, text input, metadata, optional SSE |
| `GET /agents/sessions` | Cursor pagination: `after`, `limit`, `order` |
| `GET /agents/sessions/{id}` | Current session state |
| `POST /agents/sessions/{id}` | Replace metadata |
| `DELETE /agents/sessions/{id}` | Delete an idle session and its stored checkpoint/history |
| `POST /agents/sessions/{id}/events` | One text input or cancel event; optional durable `Idempotency-Key` |
| `GET /agents/sessions/{id}/events` | Live SSE subscription |
| `GET /agents/sessions/{id}/items` | Stored messages, with cursor pagination |
| `GET /agents/sessions/{id}/turns` | Stored turns, with cursor pagination |
| `GET /agents/sessions/{id}/turns/{turn_id}` | Turn status, error, and reported usage |

Supported models follow this NanoCodex revision: `gpt-5.6-sol`, `gpt-5.6-terra`,
and `gpt-5.6-luna`. The service fixes reasoning effort at `low` and disables
priority processing. It does not expose model reasoning or internal engine events.

Tools, reusable agent CRUD/`agent_id`, images, files, artifacts, MCP, subagents,
steering, vaults, custom model settings, and hosted/self-hosted execution
environments are not implemented. Unknown request fields fail with HTTP 400.
Unknown endpoints fail with HTTP 404. Input during an active turn fails with HTTP
409. API errors use the OpenAI error envelope. These limits are part of the
current contract; SDK support does not mean full OpenAI API parity.

SSE delivers new events. It has no event replay. Subscribe before submitting
input, as the SDK `sessions.stream()` helper does. On reconnect, get stored
items and turns. A slow subscriber is disconnected if its 256-event buffer fills.
Closing a stream does not cancel a turn. Send `agent.session.input.cancel` to
cancel. Check terminal turn status; an idle session alone does not mean success.

Completed history, token totals, and the NanoCodex checkpoint commit together in
SQLite. Completed sessions survive Pod replacement with the same volume.
Interrupted turns become failed on startup and are not automatically run again.
A new turn resumes the last **completed** engine checkpoint. Failed/cancelled
turns remain in API history but do not enter subsequent model context.
Repeated event keys return success without starting duplicate work; a changed
body with the same key returns 409. Keys have session scope and remain until the
session is deleted. Session creation itself is not idempotent.

Limits: four concurrent turns, 300 seconds per turn, 200 turns per session,
1,000 sessions, 128 KiB HTTP bodies, and 64 KiB text input. There is one service
instance and one operator credential. Every client with that credential has
access to every session. Do not use this version as a public multi-tenant service
and do not increase the replica count.

## Local Kubernetes deployment

Requirements: Docker, kind, kubectl, and Bun for tests. Run commands from the
repository root. Use the explicit context to keep other clusters unchanged.

```sh
kind create cluster --name nanocodex
# If the cluster exists, reuse it.
docker build -f services/api/Dockerfile -t nanocodex-api:k8s-v1 .
kind load docker-image nanocodex-api:k8s-v1 --name nanocodex
kubectl --context kind-nanocodex apply -f deploy/k8s/base/namespace.yaml
```

Provide an existing Secret named `nanocodex-api` in namespace `nanocodex` with
fields `api-token` (32 bytes or more) and `openai-api-key`. To create it from
existing files without placing values in shell arguments:

```sh
kubectl --context kind-nanocodex -n nanocodex create secret generic nanocodex-api \
  --from-file=api-token=/absolute/path/to/service-token \
  --from-file=openai-api-key=/absolute/path/to/provider-key
kubectl --context kind-nanocodex apply -k deploy/k8s/base
kubectl --context kind-nanocodex -n nanocodex rollout status statefulset/nanocodex-api
kubectl --context kind-nanocodex -n nanocodex port-forward service/nanocodex-api 18080:8080
```

The base uses a StatefulSet, a 1 GiB volume, a ClusterIP Service, readiness and
liveness probes, resource limits, a non-root UID, a read-only container
filesystem, and no Kubernetes API token. It does not create a public ingress.
For a remote cluster, publish the image to a registry and change the image
reference in an overlay. Configure TLS at the ingress before remote use.
The default StorageClass must support `ReadWriteOnce` volumes.

Roll out a new image with a new tag. Retain the PVC during replacement. To roll
back the image, use `kubectl --context kind-nanocodex -n nanocodex rollout undo
statefulset/nanocodex-api`. This development database has no schema upgrade path.

## Contract tests with a mock model

The mock overlay runs a small Responses fixture. It performs **no real model
inference**. Use a dummy `openai-api-key` and a random local API token for it.

```sh
kubectl --context kind-nanocodex apply -k deploy/k8s/mock
bun install --cwd services/api --frozen-lockfile
# Set NANOCODEX_API_TOKEN from your local token file. Do not print it.
# Port-forward must already be active.
NANOCODEX_PROOF_FILE=/tmp/nanocodex-proof bun run --cwd services/api test
kubectl --context kind-nanocodex -n nanocodex rollout restart statefulset/nanocodex-api
kubectl --context kind-nanocodex -n nanocodex rollout status statefulset/nanocodex-api
# Restart port-forward after Pod replacement.
NANOCODEX_PROOF_FILE=/tmp/nanocodex-proof bun run services/api/tests/restart.ts
```

The tests call the actual service with the official SDK. Only the model HTTP
transport is replaced. They check live streams, retained model context,
idempotency, pagination, cancellation, provider failures, authentication,
unsupported inputs, and PVC restart recovery.

To switch an existing mock deployment to a real provider, replace the dummy key
in the Secret, apply the base, explicitly remove the `OPENAI_BASE_URL` override
if it remains on the StatefulSet, and restart the Pod. Remove the mock Deployment,
Service, and ConfigMap after it is unused. Validate a real inference call before
claiming provider integration.

## Privacy scope

This first deployment does not hide data from service or cloud operators.
Prompts, results, and model checkpoints are readable in process memory and on the
volume. The selected model provider also receives the model context. No request
content tracing is enabled by this service. The API token protects access; it is
not end-to-end encryption. Deletion removes live database records but does not
erase infrastructure backups or provider retention.

Optional VM tools, attested confidential execution, client-controlled encryption
keys, and a multi-tenant control plane are later work. See
[`MANAGED_PRIVACY_SERVICE.md`](../../docs/MANAGED_PRIVACY_SERVICE.md) for that
separate design. AX is not required for this single-service Kubernetes deployment.
