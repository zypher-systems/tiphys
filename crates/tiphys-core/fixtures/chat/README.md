# Chat Completions fixtures

Response bodies in the Chat Completions wire format, read by the tests in
`src/llm/`.

These were written by hand to the shape providers document. They are not
captures of a real provider. A wire format is not called working on the
strength of these alone: it has to pass a live tool-call round trip, and real
captures are added here as they are recorded.

| File | Holds |
| --- | --- |
| `text.sse` | A text reply with reasoning, a stop reason, usage in a final chunk, `[DONE]` |
| `parallel_tools.sse` | Two tool calls whose arguments arrive interleaved |
| `models.json` | A model list: priced, free, a router with no fixed price, unpriced, and two that are not chat models |
