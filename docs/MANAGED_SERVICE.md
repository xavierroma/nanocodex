# Managed personal agents

The product is a personal agent that the service keeps running. A user creates
an agent, connects tools or a computer, and starts work through chat or the
Agents API. The agent keeps its identity and memory across conversations and
machine changes. Machines are optional tool execution targets.

## Deployment

Use one native managed server and one persistent volume first. It owns the
API, agent runtimes, account policy, memory, and background work. Docker Compose
is the default package. One ordinary Linux VM can host it on GCP or elsewhere;
Kubernetes remains an optional deployment package. Use a separate connector
process only on computers and VMs that execute tools.

```text
App / Agents API / messaging channel
                  |
        Managed NanoCodex server
        accounts, agents, memory, jobs
                  |
       optional paired tool connection
                  |
        User computer or managed VM
```

The diagram is the target design. Today the API and memory server exist.
Account access, machine pairing, the customer interface, and messaging channels
still need implementation. An API deployment alone is not the full product.

## Reuse the existing owners

| Product need | Existing implementation | Work in this fork |
| --- | --- | --- |
| Agent loop, context, cancellation, tools | `nanocodex-agent`, `nanocodex-tools` | Already used by `services/api`; retain these owners |
| MCP and subagents | Native MCP and `nanocodex-subagents` | Already connected to the managed session API |
| Persistent personal memory | `nanocodex-memory` | Already stored by persistent agent ID; add authenticated account ownership |
| Accepted turns, recovery, event replay | `nanocodex-durability`; `services/managed` uses its journal | Integrate the native SQLite journal; replace API checkpoint-only persistence and live-only SSE |
| Account keys and provider credentials | `services/managed` account authorization; `services/egress` credential policy | Port the policy into the native service; use account-scoped keys and encrypted provider credentials |
| Commands, files, retained shell sessions | `WorkspaceToolRuntime` in `nanocodex-tools` | Reuse the runtime in an authenticated outbound machine connector |
| Isolated VM tools | Experimental `nanocodex-vm` and `examples/exe-dev` | Keep optional; connect each provisioned VM through the same machine contract |

`services/managed` (`nanocodex-managed`) is useful source material. Its
Cloudflare Durable Objects, bindings, and deployment files are host details.
The portable Rust durability crate already has SQLite and Postgres stores.
Use it instead of building a second turn scheduler or recovery loop.

Its current “Computer” is a stored virtual filesystem with an interpreted
shell. It is not a link to a user's laptop. The native workspace runtime is
the better base for that connection. Terminal and file tools do not by themselves
provide browser or desktop control; those need explicit tool integrations.

## Computer connection

The proposed connector runs on the selected computer and makes an outbound
authenticated connection. No inbound laptop port is needed. Pairing binds the
machine to an authenticated account. An agent gets an explicit grant to use
that machine and its tools. Revoke the grant to stop new dispatches.

Keep model credentials and agent memory on the server. The connector receives
only authorized tool calls and returns their results. Reuse workspace tool
definitions, execution, session polling, and process cleanup. Keep connection
authentication, dispatch, and cancellation in the application transport.

Use call IDs and retained outcomes across reconnects. If a command's outcome is
unknown, report it; do not blindly repeat a command that can have side effects.
Keep separate tool runtimes for separate agent sessions. An offline machine
must produce a clear state, not silently move execution to a different host.

A working directory does not restrict a shell's access. Use the host user's
permissions for an explicitly trusted local connector. Use a restricted OS
account, container, or VM where stronger isolation is required. A managed VM
uses the same connection contract; provisioning is an optional host function.

## Delivery order

1. **Simple private deployment:** API, persistent agent memory, Compose, and
   container replacement checks. This is the current delivery.
2. **One attached computer:** pair, grant access, run the canonical workspace
   tools, cancel a task, disconnect, and reconnect without duplicate execution.
3. **Managed customer service:** verified accounts, account-owned agents and
   machines, durable accepted turns and event replay, provider credentials,
   usage limits, backups, and a small agent/computer interface. Account policy
   must cover every read, event stream, tool call, and deletion before public use.
4. **Optional managed VMs and iMessage:** the same agent and memory behind a
   provisioned machine or verified messaging identity. Channels do not own a
   second agent loop or separate memory.

The existing OpenAI SDK support stays explicit: saved-agent sessions execute
NanoCodex on the server; the Responses endpoint lets the official Agents SDK
run its own loop. It does not automatically use managed agent memory. See the
[API support table](../services/api/README.md#api-and-sdk-support).

The present service is single-operator. It is not yet safe as a public
multi-account product. Operators and the external model provider can read task
content. Attested execution and operator-inaccessible storage remain a later
privacy milestone, separate from the simple deployment package.
