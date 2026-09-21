import { expect, test } from 'bun:test';
import OpenAI from 'openai';

const baseURL = process.env.NANOCODEX_TEST_URL ?? 'http://127.0.0.1:18080/v1';
const apiKey = process.env.NANOCODEX_API_TOKEN;
if (!apiKey) throw new Error('NANOCODEX_API_TOKEN is required');
const client = new OpenAI({ baseURL, apiKey, maxRetries: 0 });
const sessions = client.beta.agents.sessions;
type Memory = { agent_id: string; revision: number; files: Record<string, string>; pending_tasks: number };
async function memory(id: string, action = '', method = 'GET') {
  const response = await fetch(`${baseURL}/agents/${id}/memory${action}`, { method, headers: { authorization: `Bearer ${apiKey}` } });
  if (!response.ok) throw new Error(`Memory HTTP ${response.status}: ${await response.text()}`);
  return await response.json() as Memory;
}
async function run(agentID: string, text: string) {
  const session = await sessions.create({ agent_id: agentID, environment: { type: 'none' } });
  let output = '';
  for await (const event of sessions.stream(session.id, { input: text })) {
    if (event.type === 'agent.session.turn.output_text.done') output += event.text;
    if (event.type === 'agent.session.turn.failed') throw new Error(event.turn.error?.message);
  }
  await sessions.delete(session.id);
  return output;
}

test('persistent agent memory survives sessions, reconciles, and isolates agent IDs', async () => {
  const agent = await client.beta.agents.create({ model: 'gpt-5.6-sol', name: 'Memory owner' });
  expect(await run(agent.id, '[memory-write] My favorite tree is cedar.')).toContain('cedar');
  const before = await memory(agent.id);
  expect(before.pending_tasks).toBe(1);
  expect(Object.keys(before.files).some(p => p.startsWith('journal/'))).toBe(true);
  expect(before.files['profile.md']).toBeUndefined();
  const after = await memory(agent.id, '/reconcile', 'POST');
  expect(after.revision).toBeGreaterThan(before.revision);
  expect(after.pending_tasks).toBe(0);
  expect(after.files['profile.md']).toContain('cedar');
  expect(after.files['index.md']).toContain('trees');
  expect(after.files['preferences/trees.md']).toContain('cedar');
  expect(await run(agent.id, '[memory-recall] What is my favorite tree?')).toContain('Favorite tree: cedar');
  const other = await client.beta.agents.create({ model: 'gpt-5.6-sol', name: 'Other owner' });
  expect((await memory(other.id)).files).toEqual({});
  expect(await run(other.id, '[memory-recall] What do you know?')).not.toContain('Favorite tree: cedar');
  await client.beta.agents.delete(other.id);
  if (process.env.NANOCODEX_MEMORY_PROOF_FILE) await Bun.write(process.env.NANOCODEX_MEMORY_PROOF_FILE, agent.id);
  else await client.beta.agents.delete(agent.id);
});

test('inline sessions also create a reusable persistent agent ID', async () => {
  const first = await sessions.create({ agent: { model: 'gpt-5.6-sol' }, environment: { type: 'none' } });
  expect((await client.beta.agents.retrieve(first.agent.id)).id).toBe(first.agent.id);
  const second = await sessions.create({ agent_id: first.agent.id, environment: { type: 'none' } });
  expect(second.agent.id).toBe(first.agent.id);
  await sessions.delete(first.id);
  await sessions.delete(second.id);
  await client.beta.agents.delete(first.agent.id);
});

test('invalid reconciliation preserves the full prior bundle and keeps work pending', async () => {
  const agent = await client.beta.agents.create({ model: 'gpt-5.6-sol' });
  await run(agent.id, '[reject-memory] My favorite tree is cedar.');
  const before = await memory(agent.id);
  const result = await fetch(`${baseURL}/agents/${agent.id}/memory/reconcile`, { method: 'POST', headers: { authorization: `Bearer ${apiKey}` } });
  expect(result.status).toBe(400);
  expect(await result.text()).toContain("missing-record");
  expect(await memory(agent.id)).toEqual(before);
  await client.beta.agents.delete(agent.id);
});

test('cancelled turns do not commit journals or reconciliation material', async () => {
  const agent = await client.beta.agents.create({ model: 'gpt-5.6-sol' });
  const session = await sessions.create({ agent_id: agent.id, environment: { type: 'none' }, input: '[memory-cancel] My favorite tree is cedar.' });
  await Bun.sleep(400);
  await sessions.events.create(session.id, { events: [{ type: 'agent.session.input.cancel' }] });
  for (let i = 0; i < 100 && (await sessions.retrieve(session.id)).status !== 'idle'; i++) await Bun.sleep(50);
  expect((await sessions.turns.list(session.id)).data[0]?.status).toBe('cancelled');
  const stored = await memory(agent.id);
  expect(stored.files).toEqual({});
  expect(stored.pending_tasks).toBe(0);
  await sessions.delete(session.id);
  await client.beta.agents.delete(agent.id);
});


test('clearing managed memory removes records and pending material', async () => {
  const agent = await client.beta.agents.create({ model: 'gpt-5.6-sol' });
  await run(agent.id, '[memory-write] My favorite tree is cedar.');
  const response = await fetch(`${baseURL}/agents/${agent.id}/memory`, { method: 'DELETE', headers: { authorization: `Bearer ${apiKey}` } });
  expect(response.status).toBe(204);
  const cleared = await memory(agent.id);
  expect(cleared.files).toEqual({});
  expect(cleared.pending_tasks).toBe(0);
  expect(await run(agent.id, '[memory-recall] What do you know?')).not.toContain('Favorite tree: cedar');
  await client.beta.agents.delete(agent.id);
});

test('the background worker reconciles completed task memory without a manual request', async () => {
  const agent = await client.beta.agents.create({ model: 'gpt-5.6-sol' });
  try {
    await run(agent.id, '[memory-write] My favorite tree is cedar.');
    let stored = await memory(agent.id);
    const deadline = Date.now() + 70000;
    while (stored.pending_tasks > 0 && Date.now() < deadline) {
      await Bun.sleep(1000);
      stored = await memory(agent.id);
    }
    expect(stored.pending_tasks).toBe(0);
    expect(stored.files['profile.md']).toContain('cedar');
  } finally { await client.beta.agents.delete(agent.id); }
}, 75000);
