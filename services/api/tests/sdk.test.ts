import { expect, test } from 'bun:test';
import OpenAI from 'openai';
import type { AgentSessionEvent } from 'openai/resources/beta/agents/agents';

const baseURL = process.env.NANOCODEX_TEST_URL ?? 'http://127.0.0.1:18080/v1';
const apiKey = process.env.NANOCODEX_API_TOKEN;
if (!apiKey) throw new Error('Set NANOCODEX_API_TOKEN to the local service token');
const client = new OpenAI({ baseURL, apiKey, maxRetries: 0 });
const sessions = client.beta.agents.sessions;
const agent = { model: 'gpt-5.6-sol', instructions: 'Answer in plain text.' };
const environment = { type: 'none' } as const;
async function errorStatus(request: PromiseLike<unknown>, status: number) {
  try { await request; }
  catch (error) {
    expect(error).toBeInstanceOf(OpenAI.APIError);
    if (!(error instanceof OpenAI.APIError)) throw error;
    expect(error.status).toBe(status);
    return;
  }
  throw new Error(`Expected HTTP ${status}`);
}
const message = (text: string) => ({ type: 'agent.session.input.message' as const, input: [{ role: 'user' as const, content: [{ type: 'input_text' as const, text }] }] });
async function idle(id: string) {
  for (let i = 0; i < 100; i++) {
    const session = await sessions.retrieve(id);
    if (session.status === 'idle') return;
    await Bun.sleep(100);
  }
  throw new Error('Session did not reach idle');
}
function answer(events: AgentSessionEvent[]) {
  return events.filter(e => e.type === 'agent.session.turn.output_text.done').map(e => e.text).join('');
}
async function follow(id: string, input: string) {
  const events: AgentSessionEvent[] = [];
  for await (const event of sessions.stream(id, { input })) events.push(event);
  expect(events.some(e => e.type === 'agent.session.turn.completed')).toBe(true);
  return answer(events);
}

test('SDK session lifecycle, live stream, saved items, and model context', async () => {
  const events: AgentSessionEvent[] = [];
  for await (const event of await sessions.create({ agent, environment, input: 'Remember the word cedar.', stream: true })) events.push(event);
  const created = events.find(e => e.type === 'agent.session.created');
  if (!created || created.type !== 'agent.session.created') throw new Error('Missing session event');
  const id = created.session.id;
  expect(events.some(e => e.type === 'agent.session.turn.output_text.delta')).toBe(true);
  expect(events.at(-1)?.type).toBe('agent.session.idle');
  expect(answer(events)).toContain('cedar');
  const second = await follow(id, 'What word did I give you?');
  expect(second).toContain('cedar');
  expect(second).toContain('What word did I give you?');
  const turns = await sessions.turns.list(id, { order: 'asc', limit: 1 });
  expect(turns.has_more).toBe(true);
  expect((await turns.getNextPage()).data).toHaveLength(1);
  const first = await sessions.turns.retrieve(turns.data[0]!.id, { session_id: id });
  expect(first.status).toBe('completed');
  expect(first.usage?.total_tokens).toBe(15);
  const items = await sessions.items.list(id, { order: 'asc' });
  expect(items.data).toHaveLength(4);
  const updated = await sessions.update(id, { metadata: { purpose: 'restart-proof' } });
  expect(updated.metadata.purpose).toBe('restart-proof');
  const listed = await sessions.list({ limit: 100 });
  expect(listed.data.some(s => s.id === id)).toBe(true);
  if (process.env.NANOCODEX_PROOF_FILE) await Bun.write(process.env.NANOCODEX_PROOF_FILE, id);
  else await sessions.delete(id);
});

test('SDK input retry is idempotent and active input steers the current turn', async () => {
  const session = await sessions.create({ agent, environment });
  const events = [message('[slow] retry once')];
  const key = crypto.randomUUID();
  await sessions.events.create(session.id, { events, 'Idempotency-Key': key });
  await sessions.events.create(session.id, { events, 'Idempotency-Key': key });
  await errorStatus(sessions.events.create(session.id, { events: [message('different')], 'Idempotency-Key': key }), 409);
  await Bun.sleep(200);
  await sessions.events.create(session.id, { events: [message('concurrent steering')] });
  await idle(session.id);
  expect((await sessions.turns.list(session.id)).data).toHaveLength(1);
  await sessions.events.create(session.id, { events, 'Idempotency-Key': key });
  expect((await sessions.turns.list(session.id)).data).toHaveLength(1);
  await sessions.delete(session.id);
});

test('stream disconnect does not cancel; explicit input cancellation does', async () => {
  const stream = await sessions.create({ agent, environment, input: '[slow] disconnect', stream: true });
  let id = '';
  for await (const event of stream) { if (event.type === 'agent.session.created') { id = event.session.id; break; } }
  expect(id).not.toBe('');
  await idle(id);
  expect((await sessions.turns.list(id)).data[0]?.status).toBe('completed');
  await sessions.events.create(id, { events: [message('[slow] cancel')] });
  await sessions.events.create(id, { events: [{ type: 'agent.session.input.cancel' }] });
  await idle(id);
  expect((await sessions.turns.list(id)).data[0]?.status).toBe('cancelled');
  expect((await sessions.delete(id)).deleted).toBe(true);
  await errorStatus(sessions.retrieve(id), 404);
});

test('provider failure is a failed turn and leaves the session usable', async () => {
  const session = await sessions.create({ agent, environment, input: '[fail]' });
  await idle(session.id);
  const turn = (await sessions.turns.list(session.id)).data[0];
  expect(turn?.status).toBe('failed');
  expect(turn?.error?.message).not.toContain('Fixture provider failure');
  expect(await follow(session.id, 'A new valid turn')).toContain('A new valid turn');
  await sessions.delete(session.id);
});

test('auth, unknown options, unsupported environments, and validation use API errors', async () => {
  const denied = new OpenAI({ baseURL, apiKey: 'wrong', maxRetries: 0 });
  await errorStatus(denied.beta.agents.sessions.list(), 401);
  await errorStatus(sessions.create({ agent, environment: { type: 'openai_hosted' } }), 400);
  await errorStatus(sessions.create({ agent: { ...agent, tools: [{ type: 'function', name: 'exec', description: 'Test', parameters: {} }] }, environment }), 400);
  await errorStatus(sessions.create({ agent, environment, input: '' }), 400);
  await errorStatus(sessions.list({ limit: 0 }), 400);
  await errorStatus(sessions.create({ agent: { model: 'unknown' }, environment }), 400);
  expect(Array.isArray((await client.beta.agents.list()).data)).toBe(true);
});


const lookup = { type: 'function' as const, name: 'lookup', description: 'Look up a city', parameters: { type: 'object', properties: { city: { type: 'string' } }, required: ['city'], additionalProperties: false } };

test('native function tools use the SDK handler and retain calls and results', async () => {
  const session = await sessions.create({ environment, agent: { ...agent, tools: [lookup] } });
  let calls = 0;
  const events: AgentSessionEvent[] = [];
  for await (const event of sessions.stream(session.id, { input: '[native-function]', toolHandlers: { lookup: args => { expect(args.city).toBe('Barcelona'); calls++; return { forecast: 'sunny' }; } } })) events.push(event);
  expect(calls).toBe(1);
  expect(answer(events)).toContain('sunny');
  expect(events.some(e => e.type === 'agent.session.requires_action')).toBe(true);
  const items = (await sessions.items.list(session.id, { order: 'asc' })).data;
  expect(items.map(i => i.type)).toEqual(['message', 'function_call', 'function_call_output', 'message']);
  expect(items.filter(i => 'status' in i).every(i => i.status === 'completed')).toBe(true);
  const added = events.filter(e => e.type === 'agent.session.turn.item.added');
  expect(added.filter(e => e.output_index !== null).map(e => e.output_index)).toEqual([0, 1]);
  await sessions.delete(session.id);
});

test('pending calls survive client disconnect, reject wrong IDs, and accept retry-safe results', async () => {
  const session = await sessions.create({ environment, agent: { ...agent, tools: [lookup] }, input: '[native-function]' });
  let waiting = await sessions.retrieve(session.id);
  for (let i = 0; i < 100 && waiting.status !== 'requires_action'; i++) { await Bun.sleep(50); waiting = await sessions.retrieve(session.id); }
  expect(waiting.status).toBe('requires_action');
  const action = waiting.required_actions[0];
  if (!action || action.type !== 'function_call') throw new Error('No function action');
  const result = { type: 'agent.session.input.tool_result' as const, call_id: action.call_id, turn_id: action.turn_id, success: true, output: 'reconnected function result' };
  await errorStatus(sessions.events.create(session.id, { events: [{ ...result, turn_id: 'wrong' }] }), 409);
  const key = crypto.randomUUID();
  await sessions.events.create(session.id, { events: [result], 'Idempotency-Key': key });
  await sessions.events.create(session.id, { events: [result], 'Idempotency-Key': key });
  await errorStatus(sessions.events.create(session.id, { events: [{ ...result, output: 'changed' }], 'Idempotency-Key': key }), 409);
  await idle(session.id);
  expect((await sessions.items.list(session.id)).data.some(i => i.type === 'message' && i.content.some(c => c.type === 'output_text' && c.text.includes('reconnected function result')))).toBe(true);
  await sessions.delete(session.id);
});

test('function failure and cancellation both release the waiting native turn', async () => {
  const session = await sessions.create({ environment, agent: { ...agent, tools: [lookup] } });
  const events: AgentSessionEvent[] = [];
  for await (const event of sessions.stream(session.id, { input: '[native-function]', toolHandlers: { lookup: () => { throw new Error('private failure detail'); } } })) events.push(event);
  const items = (await sessions.items.list(session.id)).data;
  expect(items.some(i => i.type === 'function_call' && i.status === 'failed')).toBe(true);
  expect(JSON.stringify(items)).not.toContain('private failure detail');
  await sessions.delete(session.id);
  const pending = await sessions.create({ environment, agent: { ...agent, tools: [lookup] }, input: '[native-function]' });
  await Bun.sleep(300);
  await sessions.events.create(pending.id, { events: [{ type: 'agent.session.input.cancel' }] });
  await idle(pending.id);
  expect((await sessions.retrieve(pending.id)).required_actions).toEqual([]);
  expect((await sessions.turns.list(pending.id)).data[0]?.status).toBe('cancelled');
  await sessions.delete(pending.id);
});

test('native MCP discovery, Code Mode execution, and public call history', async () => {
  const tools = [{ type: 'mcp' as const, server_label: 'fixture', transport: { type: 'http' as const, server_url: 'http://mock-model:8081/mcp', authorization: 'Bearer fixture-private-header' }, allowed_tools: ['echo'], connection_origin: 'service' as const }];
  const session = await sessions.create({ environment, agent: { ...agent, tools } });
  expect(JSON.stringify(session)).not.toContain('fixture-private-header');
  const events: AgentSessionEvent[] = [];
  for await (const event of sessions.stream(session.id, { input: '[native-mcp]' })) events.push(event);
  expect(answer(events)).toContain('MCP echo: native MCP proof');
  const item = (await sessions.items.list(session.id)).data.find(i => i.type === 'mcp_call');
  expect(item?.type).toBe('mcp_call');
  if (item?.type === 'mcp_call') { expect(item.name).toBe('echo'); expect(item.server_label).toBe('fixture'); expect(item.status).toBe('completed'); }
  const blocked = await follow(session.id, '[blocked-mcp]');
  expect(blocked).not.toContain('MCP blocked:');
  await sessions.delete(session.id);
  await errorStatus(sessions.create({ environment, agent: { ...agent, tools: [{ ...tools[0]!, transport: { type: 'http', server_url: 'http://unlisted.invalid/mcp' } }] } }), 400);
});

test('native subagents create, run, return structured results, close, and expose child history', async () => {
  const session = await sessions.create({ environment, agent: { ...agent, multi_agent: { enabled: true, max_concurrent_subagents: 2 } } });
  const events: AgentSessionEvent[] = [];
  for await (const event of sessions.stream(session.id, { input: '[native-subagent]' })) events.push(event);
  expect(answer(events)).toContain('native child result');
  expect(events.some(e => e.type === 'agent.session.subagent.created')).toBe(true);
  expect(events.some(e => e.type === 'agent.session.subagent.closed')).toBe(true);
  const children = await sessions.subagents.list(session.id);
  expect(children.data).toHaveLength(1);
  const child = children.data[0]!;
  expect(child.status).toBe('closed');
  expect((await sessions.subagents.retrieve(child.id, { session_id: session.id })).parent_agent_id).toBe(session.agent.id);
  const turns = await sessions.subagents.turns.list(child.id, { session_id: session.id });
  expect(turns.data).toHaveLength(1);
  expect(turns.data[0]?.status).toBe('completed');
  expect(turns.data[0]?.usage?.total_tokens).toBeGreaterThan(0);
  const childItems = await sessions.subagents.turns.items.list(turns.data[0]!.id, { session_id: session.id, subagent_id: child.id });
  expect(childItems.data.some(i => i.type === 'message')).toBe(true);
  expect((await sessions.turns.list(session.id)).data).toHaveLength(1);
  const rootItems = await sessions.items.list(session.id, { order: 'asc' });
  expect(rootItems.data.map(i => i.type)).toContain('create_subagent_call');
  expect(rootItems.data.map(i => i.type)).toContain('wait_for_subagents_call');
  expect(rootItems.data.map(i => i.type)).toContain('close_subagent_call');
  await sessions.delete(session.id);
});

test('saved agents have CRUD and session snapshots retain the chosen configuration', async () => {
  const saved = await client.beta.agents.create({ ...agent, name: 'Reusable', tools: [lookup] });
  expect((await client.beta.agents.retrieve(saved.id)).name).toBe('Reusable');
  const session = await sessions.create({ environment, agent_id: saved.id });
  await client.beta.agents.update(saved.id, { name: null, instructions: 'Changed' });
  expect((await client.beta.agents.retrieve(saved.id)).name).toBeNull();
  expect((await sessions.retrieve(session.id)).agent.instructions).toBe(agent.instructions);
  await client.beta.agents.delete(saved.id);
  expect(await follow(session.id, 'Continue after saved agent deletion')).toContain('Continue after saved agent deletion');
  await sessions.delete(session.id);
});
