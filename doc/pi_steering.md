# How `pi` handles steering messages across providers

This document explains how the `pi` agent (`packages/agent`) captures and delivers
**steering** messages, and—crucially—how that delivery stays valid for every LLM
provider (`packages/ai`). The short version: steering is injected as an ordinary
`user` message at the agent-loop level (completely provider-agnostic), and the only
place where "various providers" matters is the message-transform boundary, where a
shared `transformMessages` choke point guarantees the request is still legal for
whatever provider API is downstream.

## 1. What a steering message is

A steering message is a user-supplied instruction that arrives while the agent is
already working. Rather than starting a new `prompt()` (which is rejected mid-run),
the caller queues it for delivery after the current assistant turn finishes its tool
calls, before the next LLM call.

Two related queues exist, with different timing:

| Queue | API | Delivered when |
|-------|-----|----------------|
| Steering | `agent.steer(msg)` / `sendUserMessage(text, { deliverAs: "steer" })` | After the current turn executes its tool calls, before the next LLM call |
| Follow-up | `agent.followUp(msg)` / `deliverAs: "followUp"` | Only after the agent would otherwise stop (no tool calls, no steering) |

Because follow-up is deferred until the agent is about to stop, it never interrupts a
tool-flow turn. **Only steering can land in the middle of a turn**, which is why
provider-specific handling is essentially a steering concern.

## 2. The queueing layer (no provider knowledge)

`packages/agent/src/agent.ts`:

- `Agent` owns two `PendingMessageQueue` instances: `steeringQueue` and
  `followUpQueue` (`agent.ts:174-175`).
- `steer(message)` / `followUp(message)` simply `enqueue` (`agent.ts:274-281`).
- Each queue has a `QueueMode` (`"one-at-a-time"` default, or `"all"`):
  - `one-at-a-time` — `drain()` returns a single message and waits for a response
    before the next one is delivered (`PendingMessageQueue.drain`, `agent.ts:139-152`).
  - `all` — `drain()` returns every queued message at once, delivered as one batch.
- Modes are settable at runtime via `agent.steeringMode` / `agent.followUpMode`
  (`agent.ts:255-271`), and synced from settings in the coding-agent session
  (`agent-session.ts:1739`).

`prompt()` rejects mid-run (`agent.ts:338-342`), forcing callers to use
`steer()`/`followUp()`. `continue()` drains steering first if the transcript ends on
an assistant message (`agent.ts:358-363`).

## 3. Injection in the agent loop (provider-agnostic)

`packages/agent/src/agent-loop.ts`, `runLoop` (`agent-loop.ts:155-275`):

1. At run start it polls `config.getSteeringMessages?.()` once (the
   `skipInitialSteeringPoll` path handles `continue()` draining steering itself).
2. The **inner loop** (`while (hasMoreToolCalls || pendingMessages.length > 0)`)
   executes the assistant turn's tool calls, appends their `toolResult` messages,
   emits `turn_end`, then polls steering again (`agent-loop.ts:259`).
3. Any returned steering messages are pushed to `currentContext.messages` and
   `newMessages`, emitting `message_start`/`message_end` for each
   (`agent-loop.ts:182-190`). They then precede the next `streamAssistantResponse`.
4. If the agent would otherwise stop, `getFollowUpMessages` is consulted
   (`agent-loop.ts:262-268`) before `agent_end`.

`getSteeringMessages` is wired in `createLoopConfig` (`agent.ts:458-464`), draining
the queue per its `QueueMode`. The contract is documented in `types.ts:224-235`:
messages are injected "before the next LLM call," and in the canonical path the
current turn's tool calls are **not** skipped—their results are already recorded.

`convertToLlm` (`harness/messages.ts:120`, mirrored in `coding-agent/src/core/messages.ts:148`)
turns `AgentMessage[]` → provider-neutral `Message[]`. Steering messages arrive as
`role: "user"` and pass through unchanged, so at this layer a steering message is
indistinguishable from any other user turn. The agent loop never branches on whether
a message is "steering" vs a normal prompt.

## 4. The provider boundary — where "various providers" matters

This is the heart of the question. The LLM request is built in `streamAssistantResponse`
(`agent-loop.ts:281-314`), which calls `config.convertToLlm` then hands the resulting
`Message[]` to the provider adapter via `streamFunction`. Each provider adapter
**independently** calls the shared `transformMessages` before serializing:

- `api/anthropic-messages.ts:928`
- `api/google-shared.ts:98`
- `api/openai-completions.ts:876`
- `api/openai-responses-shared.ts:130`
- `api/mistral-conversations.ts:71`
- `api/bedrock-converse-stream.ts:750`

`transformMessages` (`api/transform-messages.ts:64-223`) is the single choke point
that reconciles the conversation (which may contain a mid-run steering `user`
message) with each provider's strict conversational invariants. Its responsibilities,
and why each is provider-sensitive:

### 4.1 Orphaned tool calls → synthetic error tool results

Every provider API requires that every assistant tool call be paired, in order, with
a corresponding tool result. If a steering `user` message lands between an assistant
`toolCall` and its `toolResult`, the chain breaks and the request is rejected
(`anthropic` `tool_use` without `tool_result`, OpenAI Responses input items, Bedrock
`toolUse`/`toolResult`, etc.).

`transformMessages` tracks pending tool calls and, whenever a `user` message appears
("User message interrupts tool flow - insert synthetic results for orphaned calls",
`transform-messages.ts:210-213`) or at end of turn (`transform-messages.ts:219-220`),
inserts a synthetic result for each unmatched call:

```ts
{ role: "toolResult", toolCallId: tc.id, toolName: tc.name,
  content: [{ type: "text", text: "No result provided" }], isError: true, ... }
```

`transform-messages.ts:163-180`. So the model sees: tool call → "No result provided"
(error) → the new steering `user` message. The error text communicates that the tool
was interrupted by the steering, uniformly across providers. This is the mechanism
that makes mid-turn steering safe regardless of provider.

### 4.2 Provider-specific tool-call ID normalization

Tool-call IDs must be provider-legal. Each adapter passes its own `normalizeToolCallId`
into `transformMessages`, and the same mapping is applied to both the tool call and any
synthetic result it produces:

- Anthropic: IDs must match `^[a-zA-Z0-9_-]+$` (max 64 chars); OpenAI Responses IDs are
  450+ chars with `|` separators (`transform-messages.ts:59-63`, `anthropicNormalizeToolCallId`).
- OpenAI Responses: requires IDs prefixed `fc_`, composite `callId|itemId`
  (`openai-responses-shared.ts:121-128`).
- Mistral: `normalizeMistralToolCallId` (`mistral-conversations.ts:71`).
- Google and Bedrock: their own normalizers (`google-shared.ts`, `bedrock-converse-stream.ts`).

Because the synthetic results reuse the normalized IDs, the pairing stays valid for the
target provider.

### 4.3 Reasoning / thinking block handling

`transform-messages.ts:100-148`: redacted thinking is kept only for the same model
(opaque encrypted content is invalid cross-model); thinking with a signature is kept for
replay; otherwise thinking is converted to plain text for cross-model turns. Steering can
trigger a different turn/model replay of prior reasoning, so this keeps the request legal.

### 4.4 Image downgrade for non-vision models

`transform-messages.ts:35-57`: if the model lacks `"image"` input support, user (and
toolResult) images—including those in a steering message—are replaced with a text
placeholder. Prevents rejected requests on text-only providers.

### 4.5 Skipping broken/aborted turns

Errored or aborted assistant messages are dropped entirely (`transform-messages.ts:194-197`)
so a steering message is never paired with an incomplete tool-call turn (which would
otherwise cause "reasoning without following item" errors on OpenAI, etc.).

## 5. Summary of the cross-provider contract

- **Agent loop:** steering is a `user` message, queued by `PendingMessageQueue`, drained
  after the current turn's tool calls, injected before the next LLM call. No provider
  logic here (`agent-loop.ts`, `agent.ts`).
- **`convertToLlm`:** turns it into a neutral `user` message; no steering-specific branching.
- **Provider adapters:** all funnel through the shared `transformMessages`, which enforces
  each provider's invariants—chiefly synthesizing `isError` tool results for orphaned tool
  calls, normalizing tool-call IDs per provider, handling thinking blocks, and downgrading
  images. This is the only provider-aware steering handling.

There is **no per-provider branching on steering itself**; the uniformity comes from
inserting synthetic error tool results at the transform boundary so the interrupted
tool-use chain remains valid for Anthropic, OpenAI, Google, Bedrock, and Mistral alike.

## 6. Key references

- `packages/agent/src/agent.ts` — `PendingMessageQueue`, `steer`/`followUp`, `QueueMode`,
  `createLoopConfig.getSteeringMessages`.
- `packages/agent/src/agent-loop.ts` — `runLoop` injection of steering before next LLM call;
  `streamAssistantResponse` builds the LLM request via `convertToLlm`.
- `packages/agent/src/types.ts:224-235` — `getSteeringMessages` contract.
- `packages/agent/src/harness/messages.ts:120` and
  `packages/coding-agent/src/core/messages.ts:148` — `convertToLlm`.
- `packages/ai/src/api/transform-messages.ts` — the provider-agnostic transform with
  provider-specific `normalizeToolCallId`, synthetic orphaned tool results, thinking/image
  handling.
- `packages/ai/src/api/{anthropic-messages,google-shared,openai-completions,openai-responses-shared,mistral-conversations,bedrock-converse-stream}.ts`
  — adapters that invoke `transformMessages`.
