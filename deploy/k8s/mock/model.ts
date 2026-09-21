// Deterministic model and MCP transports. No inference occurs in this fixture.
type Content = { type: string; text?: string };
type InputItem = { type?: string; role?: string; name?: string; call_id?: string; arguments?: string; input?: string; output?: string | Content[]; content?: string | Content[]; tools?: ModelTool[] };
type ModelTool = { type: string; name?: string; tools?: ModelTool[] };
type ModelRequest = { model?: string; instructions?: string; input?: string | InputItem[]; tools?: ModelTool[]; stream?: boolean; previous_response_id?: string };
type McpRequest = { jsonrpc: string; id?: string | number; method: string; params?: { name?: string; arguments?: { text?: string } } };
const history = new Map<string, InputItem[]>();
const textOf = (content?: string | Content[]) => typeof content === 'string' ? content : content?.map(c => c.text ?? '').join(' ') ?? '';
const message = (text: string) => ({ id: `msg_${crypto.randomUUID()}`, type: 'message' as const, role: 'assistant' as const, status: 'completed' as const, content: [{ type: 'output_text' as const, text, annotations: [] }] });
const call = (name: string, args: unknown) => ({ id: `fc_${crypto.randomUUID()}`, type: 'function_call' as const, call_id: `call_${crypto.randomUUID()}`, name, arguments: JSON.stringify(args), status: 'completed' as const });
const custom = (name: string, input: string) => ({ id: `ct_${crypto.randomUUID()}`, type: 'custom_tool_call' as const, call_id: `call_${crypto.randomUUID()}`, name, input, status: 'completed' as const });
type Output = ReturnType<typeof message> | ReturnType<typeof call> | ReturnType<typeof custom>;

Bun.serve({
  port: Number(process.env.PORT ?? 8081),
  async fetch(request) {
    const path = new URL(request.url).pathname;
    if (path === '/mcp') {
      if (request.method !== 'POST') return new Response(null, { status: request.method === 'DELETE' ? 200 : 405 });
      const rpc = await request.json() as McpRequest;
      if (rpc.id === undefined) return new Response(null, { status: 202 });
      let result: unknown;
      if (rpc.method === 'initialize') result = { protocolVersion: '2025-03-26', capabilities: { tools: {} }, serverInfo: { name: 'contract-fixture', version: '1.0.0' } };
      else if (rpc.method === 'tools/list') result = { tools: ['echo', 'blocked'].map(name => ({ name, description: `Fixture ${name}`, inputSchema: { type: 'object', properties: { text: { type: 'string' } }, required: ['text'], additionalProperties: false } })) };
      else if (rpc.method === 'tools/call') result = { content: [{ type: 'text', text: `MCP ${rpc.params?.name}: ${rpc.params?.arguments?.text}` }], isError: false };
      else if (rpc.method === 'ping') result = {};
      else return Response.json({ jsonrpc: '2.0', id: rpc.id, error: { code: -32601, message: 'Unknown method' } });
      return Response.json({ jsonrpc: '2.0', id: rpc.id, result });
    }
    if (path !== '/v1/responses') return new Response('Not found', { status: 404 });
    const body = await request.json() as ModelRequest;
    const input = [...(history.get(body.previous_response_id ?? '') ?? []), ...(typeof body.input === 'string' ? [{ role: 'user', content: body.input }] : body.input ?? [])];
    const users = input.filter(m => m.role === 'user');
    const allText = users.map(m => textOf(m.content)).join('\n');
    const instructions = body.instructions ?? '';
    const currentText = textOf(users.at(-1)?.content);
    if (textOf(users.at(-1)?.content).includes('[fail]')) return Response.json({ error: { message: 'Fixture provider failure', type: 'authentication_error' } }, { status: 401 });
    const currentInput = input.slice(input.findLastIndex(i => i.role === 'user') + 1);
    const resultFor = (name: string) => {
      const invocation = currentInput.findLast(i => i.name === name && (i.type === 'function_call' || i.type === 'custom_tool_call'));
      return invocation ? input.findLast(i => i.call_id === invocation.call_id && (i.type === 'function_call_output' || i.type === 'custom_tool_call_output')) : undefined;
    };
    const toolNames = (tools: ModelTool[]): (string | undefined)[] => tools.flatMap(t => [t.name, ...toolNames(t.tools ?? [])]);
    const names = toolNames([...(body.tools ?? []), ...input.filter(i => i.type === 'additional_tools').flatMap(i => i.tools ?? [])]);
    let output: Output[];
    if (names.includes('memory_write')) {
      const date = /memory for (\d{4}-\d{2}-\d{2})/.exec(allText)?.[1] ?? new Date().toISOString().slice(0, 10);
      const writes = input.filter(i => i.type === 'function_call' && i.name === 'memory_write');
      const failed = allText.includes('[reject-memory]');
      const files = [
        { path: 'profile.md', content: `# Profile\n\n- Favorite tree: cedar (${date}).\n` },
        { path: 'preferences/trees.md', content: `---\nid: trees\ntype: preference\naliases: [tree, cedar]\nupdated: ${date}\n---\n- Favorite tree is cedar; said so on ${date}.${failed ? ' [[missing-record]]' : ''}\n` },
        { path: `log/${date}.md`, content: `- ${date}: Recorded the stated tree preference from the completed task.\n` },
      ];
      output = writes.length < files.length ? [call('memory_write', files[writes.length])] : [message('Memory reconciled')];
    } else if (currentText.includes('[memory-write]') || currentText.includes('[memory-cancel]') || currentText.includes('[reject-memory]')) {
      output = resultFor('memory_journal') ? [message('Remembered the preference for cedar.')] : [call('memory_journal', { note: 'The user said their favorite tree is cedar.' })];
    } else if (currentText.includes('[memory-recall]')) {
      if (!resultFor('memory_read')) output = [call('memory_read', { path: 'profile.md' })];
      else if (!resultFor('memory_search')) output = [call('memory_search', { query: 'cedar' })];
      else output = [message(`Memory recall: ${textOf(resultFor('memory_read')?.output)}; search: ${textOf(resultFor('memory_search')?.output)}`)];
    } else if (currentText.includes('[native-child]')) {
      if (!resultFor('submit_result')) {
        const token = /turn_token: (\d+)/.exec(instructions + allText)?.[1];
        output = [call('submit_result', { turn_token: Number(token ?? 1), output: { report: 'native child result' } })];
      } else output = [message('native child done')];
    } else if (currentText.includes('[native-subagent]')) {
      if (!resultFor('spawn_agent')) output = [call('spawn_agent', { role: 'researcher', task: '[native-child] Return the report.', output_schema: { type: 'object', properties: { report: { type: 'string' } }, required: ['report'], additionalProperties: false } })];
      else if (!resultFor('wait_agent')) output = [call('wait_agent', { agent_ids: [1], timeout_ms: 10000 })];
      else if (!resultFor('close_agent')) output = [call('close_agent', { agent_id: 1 })];
      else output = [message(`Native delegation complete: ${textOf(resultFor('wait_agent')?.output)}`)];
    } else if (currentText.includes('[native-mcp]') || currentText.includes('[blocked-mcp]')) {
      if (!resultFor('tool_search') && !input.some(i => i.type === 'tool_search_output')) output = [call('tool_search', { query: 'echo', limit: 2 })];
      else if (!resultFor('exec')) output = [custom('exec', currentText.includes('[blocked-mcp]') ? 'text(await tools.mcp__fixture__blocked({text:"must not execute"}));' : 'const result = await tools.mcp__fixture__echo({text:"native MCP proof"}); text(result);')];
      else output = [message(`Native MCP result: ${textOf(resultFor('exec')?.output)}`)];
    } else if (currentText.includes('[native-function]') || currentText.includes('[sdk-function]')) {
      if (!resultFor('lookup')) output = [call('lookup', { city: 'Barcelona' })];
      else output = [message(`Function result: ${textOf(resultFor('lookup')?.output)}`)];
    } else if (currentText.includes('[sdk-handoff]')) {
      const handoff = names.find(n => n?.startsWith('transfer_to_'));
      output = handoff ? [call(handoff, {})] : [message('Specialist completed the handoff')];
    } else if (currentText.includes('[sdk-nested]')) {
      if (names.includes('specialist') && !resultFor('specialist')) output = [call('specialist', { input: 'Return Nested specialist result' })];
      else if (names.includes('specialist')) output = [message(`Nested result: ${textOf(resultFor('specialist')?.output)}`)];
      else output = [message('Nested specialist result')];
    } else if (currentText.includes('[sdk-mcp]')) {
      if (!resultFor('echo')) output = [call('echo', { text: 'SDK MCP proof' })];
      else output = [message(`SDK MCP result: ${textOf(resultFor('echo')?.output)}`)];
    } else output = [message(`Mock model received ${users.length} user messages: ${allText}`)];
    const responseID = `resp_${crypto.randomUUID()}`;
    const response = { id: responseID, object: 'response', created_at: Math.floor(Date.now() / 1000), status: 'completed', model: body.model ?? 'gpt-5.6-sol', error: null, incomplete_details: null, instructions: null, metadata: {}, output, parallel_tool_calls: true, tool_choice: 'auto', tools: body.tools ?? [], temperature: 1, top_p: 1, max_output_tokens: null, previous_response_id: body.previous_response_id ?? null, reasoning: { effort: null, summary: null }, store: false, text: { format: { type: 'text' } }, truncation: 'disabled', usage: { input_tokens: 10, output_tokens: 5, total_tokens: 15, input_tokens_details: { cached_tokens: 0 }, output_tokens_details: { reasoning_tokens: 0 } } };
    history.set(responseID, [...input, ...output]);
    if (history.size > 1000) history.delete(history.keys().next().value!);
    if (!body.stream) return Response.json(response);
    const stream = new ReadableStream<Uint8Array>({
      async start(controller) {
        let sequence = 0;
        const send = (event: unknown) => controller.enqueue(new TextEncoder().encode(`data: ${JSON.stringify({ sequence_number: sequence++, ...event as Record<string, unknown> })}\n\n`));
        try {
          send({ type: 'response.created', response: { ...response, status: 'in_progress', output: [] } });
          if (currentText.includes('[slow]') || (currentText.includes('[memory-cancel]') && resultFor('memory_journal'))) await Bun.sleep(2000);
          for (const [index, item] of output.entries()) {
            send({ type: 'response.output_item.added', output_index: index, item: { ...item, status: 'in_progress', ...(item.type === 'message' ? { content: [] } : item.type === 'function_call' ? { arguments: '' } : { input: '' }) } });
            if (item.type === 'message') {
              send({ type: 'response.content_part.added', item_id: item.id, output_index: index, content_index: 0, part: { type: 'output_text', text: '', annotations: [] } });
              send({ type: 'response.output_text.delta', item_id: item.id, output_index: index, content_index: 0, delta: item.content[0]!.text });
              send({ type: 'response.output_text.done', item_id: item.id, output_index: index, content_index: 0, text: item.content[0]!.text });
              send({ type: 'response.content_part.done', item_id: item.id, output_index: index, content_index: 0, part: item.content[0] });
            } else if (item.type === 'function_call') {
              send({ type: 'response.function_call_arguments.delta', item_id: item.id, output_index: index, delta: item.arguments });
              send({ type: 'response.function_call_arguments.done', item_id: item.id, output_index: index, arguments: item.arguments });
            }
            send({ type: 'response.output_item.done', output_index: index, item });
          }
          send({ type: 'response.completed', response });
          controller.close();
        } catch { /* Disconnect closes the stream. */ }
      },
    });
    return new Response(stream, { headers: { 'content-type': 'text/event-stream' } });
  },
});
