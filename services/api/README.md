# NanoCodex managed API

A Rust HTTP service for NanoCodex, with a Kubernetes deployment and persistent
agent memory. A VM is optional. The current deployment uses no VM.

## API and SDK support

Two interfaces have different owners:

| Interface | Execution owner | Tested behavior |
| --- | --- | --- |
| `client.beta.agents` in `openai@7.15.0` | This service runs NanoCodex | Saved agents, sessions, retained context, streaming, steering, cancellation, client functions, native MCP, native subagents, child history |
| `Runner` in `@openai/agents@0.18.0` | The SDK runs the agent loop; this service forwards `POST /v1/responses` to the model provider | Streaming and nonstreaming functions, handoffs, agent-as-tool, MCP, input guardrails |
| `/v1/agents/{agent_id}/memory` | This service | Persistent records, inspection, clearing, and reconciliation |

The Responses route preserves the provider's request, output, and stream. It
uses the server's model credential. It does not convert an SDK Runner into a
server-owned NanoCodex session. Managed memory applies to the saved-agent/session
interface. SDK Runner applications must select and manage their own session or
memory integration.

This is **not full OpenAI platform parity**. The service does not implement
files, hosted execution environments, vaults, realtime, audio, image inputs,
Chat Completions, the tracing backend, or Responses retrieval/deletion routes.
The session API accepts text and `environment: { type: 'none' }`. Deferred client
functions and required MCP startup checks are rejected. Native MCP discovery is
supported. HTTP MCP is tested; stdio MCP is implemented but not tested here.
Unsupported fields and routes return errors.

## Use a persistent agent

```typescript
import OpenAI from 'openai';

const client = new OpenAI({
  baseURL: 'http://127.0.0.1:18080/v1',
  apiKey: process.env.NANOCODEX_API_TOKEN,
});
const agent = await client.beta.agents.create({
  model: 'gpt-5.6-sol',
  name: 'Personal assistant',
  instructions: 'Help with daily tasks.',
});
// Save agent.id in your application. Use it for each new conversation.
const session = await client.beta.agents.sessions.create({
  agent_id: agent.id,
  environment: { type: 'none' },
});
for await (const event of client.beta.agents.sessions.stream(session.id, {
  input: 'Remember that my favorite tree is cedar.',
})) {
  if (event.type === 'agent.session.turn.output_text.delta') {
    process.stdout.write(event.delta);
  }
}
```

Creating a session with an inline `agent` also saves that agent. Its returned
`session.agent.id` can be reused. A session keeps its configuration snapshot when
the saved agent is changed or deleted.

## Functions, MCP, and subagents

Declare a function in `agent.tools`, with `type: 'function'`, `name`,
`description`, and a JSON Schema `parameters` object. Pass a handler under
`toolHandlers` to `sessions.stream`. The native turn waits for the handler's
result. For a custom client, read `required_actions`, then submit
`agent.session.input.tool_result` with the matching `turn_id` and `call_id`.
An `Idempotency-Key` permits a safe retry. Cancellation clears pending calls.

An HTTP MCP tool has this shape:

```typescript
const mcp = {
  type: 'mcp' as const,
  server_label: 'docs',
  transport: { type: 'http' as const, server_url: 'https://mcp.example.com/mcp' },
  connection_origin: 'service' as const,
  allowed_tools: ['search'],
};
```

The operator must put the exact URL in `NANOCODEX_MCP_ALLOWED_URLS` (comma-separated).
No MCP destinations are allowed by default. `NANOCODEX_MCP_ALLOWED_COMMANDS`
provides the corresponding stdio command allowlist. Only configure trusted
stdio programs: their arguments and environment are supplied by the API caller.
Transport credentials are stored with the configuration but removed from public
agent/session responses. Native MCP uses tool discovery and Code Mode; call
results appear in session history.

Set `multi_agent: { enabled: true, max_concurrent_subagents: 6 }` for native
spawn, message, list, wait, interrupt, and close tools. Children inherit MCP and
read-only agent memory. Client functions remain on the root agent. The session's
`subagents` routes expose child resources, turns, usage, and items. A cancelled
root interrupts its children; deleting a session closes its runtime.

## Use the official Agents SDK

```typescript
import { Agent, Runner, OpenAIProvider, setTracingDisabled } from '@openai/agents';

setTracingDisabled(true);
const runner = new Runner({
  modelProvider: new OpenAIProvider({
    baseURL: 'http://127.0.0.1:18080/v1',
    apiKey: process.env.NANOCODEX_API_TOKEN,
    useResponses: true,
  }),
  tracingDisabled: true,
});
const specialist = new Agent({ name: 'Specialist', model: 'gpt-5.6-sol' });
const coordinator = new Agent({
  name: 'Coordinator', model: 'gpt-5.6-sol', handoffs: [specialist],
});
const result = await runner.run(coordinator, 'Ask the specialist for help.');
console.log(result.finalOutput);
```

The SDK executes handoffs, local functions, and local MCP clients. Their network
access is controlled by the application host. The service's MCP allowlist applies
to the server-owned session interface.

## Managed memory

`nanocodex-memory` is an MIT-licensed workspace crate. Each persistent agent ID
owns a bundle of Markdown files: `profile.md`, generated `index.md`, typed
records, dated journals, and an audit log. The API stores bundles, prior versions,
and completed task material in SQLite on the PVC.

The root agent gets `memory_read`, `memory_search`, and `memory_journal` by
default. Profile and index are supplied as untrusted evidence on each new turn.
Journal notes are staged until successful completion. Failed and cancelled turns
do not add reconciliation material. Notes also remain in completed task material
if the dated journal has reached its size limit.

The worker checks pending work every 60 seconds. Each pass processes up to 20
tasks and approximately 512 KiB of source material per agent. It uses the saved
agent's model and only memory tools. A separate draft receives edits. Validation
checks file structure, dates, links, size limits, the profile, and the audit log;
it then generates the index. A transaction accepts the complete bundle only if
its stored revision has not changed. Invalid drafts leave prior memory intact.
The validator checks structure, not factual truth. This process makes additional
model calls.

Authenticated extensions:

- `GET /v1/agents/{agent_id}/memory`: files, revision, pending task count.
- `POST /v1/agents/{agent_id}/memory/reconcile`: process one batch now. A concurrent
  job or revision change returns 409. Retry after the other operation ends.
- `DELETE /v1/agents/{agent_id}/memory`: clear files, versions, and source material.
  Active turns must first stop. Existing conversation histories are separate and
  can still contain remembered facts; delete those sessions when removing data.

Deleting a conversation retains agent memory. Deleting the saved agent and its
last conversation removes the stored memory and reconciliation material. There
is no automatic expiry. Operators must manage retention and backups.

## Kubernetes

The deployment is a single-replica StatefulSet with a 1 GiB PVC, a non-root user,
a read-only root filesystem, and no service-account token. Do not scale this
SQLite deployment to multiple replicas. Root turns and Responses calls share four
execution slots. Sessions are limited to 200 turns; the service admits up to
1,000 sessions and 1,000 saved agents. Root turns stop after 300 seconds.

For the local fixture deployment, from the repository root:

```sh
docker build -f services/api/Dockerfile -t nanocodex-api:k8s-v4 .
kind load docker-image nanocodex-api:k8s-v4 --name nanocodex
kubectl --context kind-nanocodex apply -k deploy/k8s/mock
kubectl --context kind-nanocodex -n nanocodex rollout status statefulset/nanocodex-api
kubectl --context kind-nanocodex -n nanocodex port-forward service/nanocodex-api 18080:8080
```

Create the `nanocodex-api` Secret in the namespace first, with `api-token` and
`openai-api-key`. Keep real credentials out of shell history and Git. The fixture
uses a dummy provider key. The base deployment uses `https://api.openai.com/v1`;
`OPENAI_BASE_URL` can select another compatible endpoint. Applying the base over
the mock deployment can leave old environment overrides; remove the mock URL
explicitly before using a real credential.

On restart, persisted completed context and memory are restored. Unfinished
turns become failed, pending functions are cleared, and old child runtimes close.
The runtime resumes the last successful checkpoint. During a live session, the
native runtime controls retention of partial failed or cancelled work.
SSE is live and has no replay. Connect before submitting input; use stored items
and turns to recover after a disconnect. Disconnecting a stream does not cancel
its turn.

## Verification

```sh
docker build -f services/api/Dockerfile --target test -t nanocodex-api:checks .
# In a second terminal, expose the fixture for the SDK-owned MCP test:
kubectl --context kind-nanocodex -n nanocodex port-forward service/mock-model 18081:8081
cd services/api
bun install --frozen-lockfile
bun run test
```

Set `NANOCODEX_API_TOKEN` to the service token before testing. Optional
`NANOCODEX_PROOF_FILE` and `NANOCODEX_MEMORY_PROOF_FILE` keep resource IDs for
`bun tests/restart.ts` after a pod restart. Both must be absolute paths outside
the checkout. The tests use the real SDKs, HTTP API, NanoCodex runtime, MCP
transport, SQLite, and Kubernetes. Only model inference is replaced with a
local deterministic transport. Passing these tests does not prove real-provider
behavior or complete SDK parity.

## Privacy scope

This version has one operator credential and no tenant authorization boundary.
An agent ID separates memory records; it is not an access credential. This is a
local deployment prototype, not a public multi-tenant service.

Prompts, results, memory, checkpoints, and configured MCP credentials are readable
by the service and storage operators. The external model provider receives model
context, including memory needed for a task or reconciliation. No content tracing
is enabled by this service. Disable SDK tracing in client applications as shown
above. Enclaves, attestation, encrypted VMs, and end-to-end encryption are not
implemented. Database deletion does not erase infrastructure backups or provider
retention.
