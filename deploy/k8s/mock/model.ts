// Test transport only. No inference occurs here. Never use this overlay for real model calls.
type Content = { type: string; text?: string };
type Message = { role?: string; content?: string | Content[] };
type ModelRequest = { input?: Message[]; tools?: unknown[]; generate?: boolean };
Bun.serve({
  port: Number(process.env.PORT ?? 8081),
  async fetch(request) {
    if (new URL(request.url).pathname !== '/v1/responses') return new Response('Not found', { status: 404 });
    const body = await request.json() as ModelRequest;
    if (body.tools?.length) return new Response('Tools must be empty', { status: 400 });
    const input = (body.input ?? []).filter(m => m.role === 'user');
    const allText = input.map(m => typeof m.content === 'string' ? m.content : m.content?.map(c => c.text ?? '').join(' ')).join('\n');
    if (allText.includes('[fail]')) return new Response(JSON.stringify({ error: { message: 'Fixture provider failure', type: 'authentication_error' } }), { status: 401, headers: { 'content-type': 'application/json' } });
    const text = `Mock model received ${input.length} user messages: ${allText}`;
    const responseID = `resp_${crypto.randomUUID()}`;
    const itemID = `msg_${crypto.randomUUID()}`;
    const stream = new ReadableStream<Uint8Array>({
      async start(controller) {
        const send = (event: unknown) => controller.enqueue(new TextEncoder().encode(`data: ${JSON.stringify(event)}\n\n`));
        try {
          send({ type: 'response.created', response: { id: responseID, status: 'in_progress', output: [] } });
          if (allText.includes('[slow]')) await Bun.sleep(5000);
          send({ type: 'response.output_text.delta', item_id: itemID, output_index: 0, content_index: 0, delta: text });
          send({ type: 'response.completed', response: { id: responseID, status: 'completed', output: [{ id: itemID, type: 'message', role: 'assistant', content: [{ type: 'output_text', text }] }], usage: { input_tokens: 10, output_tokens: 5, total_tokens: 15 } } });
          controller.close();
        } catch { /* Client cancellation closes the fixture stream. */ }
      },
    });
    return new Response(stream, { headers: { 'content-type': 'text/event-stream' } });
  },
});
