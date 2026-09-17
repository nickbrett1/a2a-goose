# The turn-deadline extension (`turn-deadline/v1`)

An a2a-goose agent advertises how long **one turn** may take, in its card, under
`capabilities.extensions`:

```json
{
  "uri": "https://github.com/nickbrett1/a2a-goose/blob/main/docs/turn-deadline.md",
  "description": "One turn on this agent may take up to 900 seconds, and a cancel is acknowledged within 10. …",
  "required": false,
  "params": { "promptSecs": 900, "cancelSecs": 10 }
}
```

`params.promptSecs` is `goose.acp.timeouts.promptSecs` — the ceiling the agent
actually enforces on a whole turn — and `cancelSecs` is
`goose.acp.timeouts.cancelSecs`. The card is hashed, so changing either one
re-registers the agent.

## Why it is an extension and not a field

The A2A spec has no field for a server-side turn budget. `capabilities.extensions`
is the spec's own place for a fact that a client may safely ignore, which is
exactly the case here: `required` is `false`, so a client that has never heard of
this URI behaves as before. The `description` is there for a model that reads the
card, and `params` for a caller that wants to act without parsing prose.

It is **advertising, not enforcement** — the same rule as `securitySchemes` in
`src/card.rs`. The deadline is enforced by the ACP transport (`promptSecs`); the
card only tells a caller what it is.

## What a caller should do with it

- **Size your own tool-call timeout from the task, not from this number.** The
  number is a ceiling, not a cost. Measured on mac-studio: a new conversation on
  a running agent answers in ~2 s, the first turn after a restart in ~2 s, and
  three simultaneous turns in ~3 s ([S18](../spikes/S18.md)). There is no warm-up
  to budget for.
- **A call that gives up does not stop the turn.** The agent carries on with
  nobody listening — measured through a hub tool call (`MCP error -32001`) and
  through `curl` at 240 s, both of which lost answers to turns that completed.
- **So agent-to-agent calls are for relatively short-lived work.** If a task will
  outlive the call, ask for the result to be written down somewhere durable (a
  memo, a file) *before* the agent explains, then collect it on a later call with
  the same `contextId`.

Both of the tool surfaces that hand work to an agent say this where the model
will read it: `integrations/a2a-mcp/` (the MCP tools every mcphub group carries)
and `integrations/openwebui/` (the agent as an Open WebUI model).
