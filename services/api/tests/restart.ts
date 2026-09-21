import assert from 'node:assert/strict';
import OpenAI from 'openai';
const proof = process.env.NANOCODEX_PROOF_FILE;
if (!proof) throw new Error('NANOCODEX_PROOF_FILE is required');
const id = (await Bun.file(proof).text()).trim();
const apiKey = process.env.NANOCODEX_API_TOKEN;
if (!apiKey) throw new Error('NANOCODEX_API_TOKEN is required');
const client = new OpenAI({ apiKey, baseURL: process.env.NANOCODEX_TEST_URL ?? 'http://127.0.0.1:18080/v1', maxRetries: 0 });
const sessions = client.beta.agents.sessions;
const saved = await sessions.retrieve(id);
assert.equal(saved.metadata.purpose, 'restart-proof');
assert.equal((await sessions.turns.list(id)).data.length, 2);
let output = '';
for await (const event of sessions.stream(id, { input: 'Check context after the pod restart.' })) {
  if (event.type === 'agent.session.turn.output_text.done') output += event.text;
}
assert.match(output, /cedar/);
assert.match(output, /Check context after the pod restart/);
assert.equal((await sessions.items.list(id)).data.length, 6);
await sessions.delete(id);
console.log('PASS: PVC history and NanoCodex model context survived the pod restart');

const memoryProof = process.env.NANOCODEX_MEMORY_PROOF_FILE;
if (!memoryProof) throw new Error('NANOCODEX_MEMORY_PROOF_FILE is required');
const agentID = (await Bun.file(memoryProof).text()).trim();
assert.equal((await client.beta.agents.retrieve(agentID)).name, 'Memory owner');
const recalled = await sessions.create({ agent_id: agentID, environment: { type: 'none' } });
let memoryOutput = '';
for await (const event of sessions.stream(recalled.id, { input: '[memory-recall] What is my favorite tree?' })) {
  if (event.type === 'agent.session.turn.output_text.done') memoryOutput += event.text;
  if (event.type === 'agent.session.turn.failed') throw new Error(event.turn.error?.message);
}
assert.match(memoryOutput, /Favorite tree: cedar/);
await sessions.delete(recalled.id);
await client.beta.agents.delete(agentID);
console.log('PASS: saved agent, curated memory, and memory tool recall survived the pod restart');
