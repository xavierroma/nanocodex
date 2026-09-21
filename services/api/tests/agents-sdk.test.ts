import { expect, test } from 'bun:test';
import { z } from 'zod';
import { Agent, Runner, OpenAIProvider, tool, setTracingDisabled, MCPServerStreamableHttp, InputGuardrailTripwireTriggered } from '@openai/agents';

const baseURL = process.env.NANOCODEX_TEST_URL ?? 'http://127.0.0.1:18080/v1';
const apiKey = process.env.NANOCODEX_API_TOKEN;
if (!apiKey) throw new Error('NANOCODEX_API_TOKEN is required');
setTracingDisabled(true);
const provider = new OpenAIProvider({ baseURL, apiKey, useResponses: true });
const config = { modelProvider: provider, tracingDisabled: true };
const runner = new Runner(config);
const model = 'gpt-5.6-sol';

test('official Agents SDK calls functions with nonstreaming and streaming Responses', async () => {
  let executions = 0;
  const lookup = tool({ name: 'lookup', description: 'Look up a city', parameters: z.object({ city: z.string() }), execute: ({ city }) => { executions++; return `weather in ${city}: sunny`; } });
  const agent = new Agent({ name: 'Weather', model, tools: [lookup] });
  const result = await runner.run(agent, '[sdk-function]');
  expect(result.finalOutput).toContain('weather in Barcelona: sunny');
  const stream = await runner.run(agent, '[sdk-function]', { stream: true });
  const eventTypes = new Set<string>();
  for await (const event of stream) eventTypes.add(event.type);
  expect(stream.finalOutput).toContain('weather in Barcelona: sunny');
  expect(executions).toBe(2);
  expect(eventTypes.has('raw_model_stream_event')).toBe(true);
  expect(eventTypes.has('run_item_stream_event')).toBe(true);
});

test('official Agents SDK performs a handoff and exposes the final specialist', async () => {
  const specialist = new Agent({ name: 'Specialist', model });
  const coordinator = new Agent({ name: 'Coordinator', model, handoffs: [specialist] });
  const result = await runner.run(coordinator, '[sdk-handoff]');
  expect(result.lastAgent?.name).toBe('Specialist');
  expect(result.finalOutput).toBe('Specialist completed the handoff');
  expect(result.newItems.some(i => i.type === 'handoff_output_item')).toBe(true);
});

test('official Agents SDK runs a nested agent as a tool through the same API', async () => {
  const specialist = new Agent({ name: 'Specialist', model });
  const coordinator = new Agent({ name: 'Coordinator', model, tools: [specialist.asTool({ toolName: 'specialist', toolDescription: 'Ask the specialist', runConfig: config })] });
  const result = await runner.run(coordinator, '[sdk-nested]');
  expect(result.finalOutput).toContain('Nested specialist result');
  expect(result.lastAgent?.name).toBe('Coordinator');
});

test('official Agents SDK runs an MCP tool and passes its result through Responses', async () => {
  const server = new MCPServerStreamableHttp({ name: 'fixture', url: process.env.NANOCODEX_TEST_MCP_URL ?? 'http://127.0.0.1:18081/mcp' });
  await server.connect();
  try {
    const agent = new Agent({ name: 'MCP user', model, mcpServers: [server] });
    const result = await runner.run(agent, '[sdk-mcp]');
    expect(result.finalOutput).toContain('MCP echo: SDK MCP proof');
  } finally { await server.close(); }
});

test('official Agents SDK retains its input guardrail behavior', async () => {
  const agent = new Agent({ name: 'Guarded', model, inputGuardrails: [{ name: 'block', runInParallel: false, execute: async () => ({ outputInfo: { reason: 'test' }, tripwireTriggered: true }) }] });
  try { await runner.run(agent, 'blocked'); throw new Error('Expected guardrail rejection'); }
  catch (error) { expect(error).toBeInstanceOf(InputGuardrailTripwireTriggered); }
});
