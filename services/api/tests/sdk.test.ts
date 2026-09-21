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

test('SDK input retry is idempotent; changed body and concurrent input fail', async () => {
  const session = await sessions.create({ agent, environment });
  const events = [message('[slow] retry once')];
  const key = crypto.randomUUID();
  await sessions.events.create(session.id, { events, 'Idempotency-Key': key });
  await sessions.events.create(session.id, { events, 'Idempotency-Key': key });
  await errorStatus(sessions.events.create(session.id, { events: [message('different')], 'Idempotency-Key': key }), 409);
  await errorStatus(sessions.events.create(session.id, { events: [message('concurrent')] }), 409);
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
  await errorStatus(sessions.create({ agent: { ...agent, tools: [{ type: 'function', name: 'unsupported', description: 'Test', parameters: {} }] }, environment }), 400);
  await errorStatus(sessions.create({ agent, environment, input: '' }), 400);
  await errorStatus(sessions.list({ limit: 0 }), 400);
  await errorStatus(sessions.create({ agent: { model: 'unknown' }, environment }), 400);
  await errorStatus(client.beta.agents.list(), 404);
});
