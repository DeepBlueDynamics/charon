# 07 — Consumer & Nemesis8

Satisfies objective (b): *a consumer uses Nemesis8 to fire up any coding agent
and point it at a shared model.*

## The consumer proxy

A local process that presents a **plain OpenAI-compatible API** to any agent and
does all the marketplace work behind it. The agent stays oblivious to NUTS,
encryption, and sats.

### Surfaces (to the agent, localhost)
- `POST /v1/chat/completions` (streaming + buffered) — the main path.
- `GET  /v1/models` — the consumer's resolvable models (from pins/directory).
- `POST /v1/estimate-cost` — price preview (05).

### Holds
- The consumer's NUTS `ahp_` token and X25519 static key + signed binding (02).
- A **wallet** for paying (Cashu/LN/balance).
- A **trusted-provider pin set** `{provider → x25519_pub}` (02, 08).
- A **model → provider** resolution table (which trusted provider serves a name;
  if several, choose by price/latency/rating — this is the consumer-side routing
  that replaces server-side routing).

### Per-request behavior (normative)
1. Receive an OpenAI request from the agent.
2. Resolve `model` → a trusted provider (pin set). If none, `404`/`no_provider`.
3. Quote price (05); if a budget guard is set and exceeded, reject locally.
4. Open the gateway session: send the cleartext envelope + payment (03, 05).
5. Run the Noise handshake to the provider's **pinned** key (04); abort on key
   mismatch.
6. Encrypt the request body; relay; decrypt the streamed reply; re-emit it to
   the agent as normal OpenAI SSE.
7. Optionally publish a signed rating (08).

### Budget guards (RECOMMENDED)
Per-request and per-session sat ceilings, rejecting before payment. This is the
guardrail against an agent burning funds on an expensive model in a loop.

## Nemesis8 integration

Nemesis8 runs each agent session in a **sealed Docker container** with
filesystem/network/shell scoped at spawn, and connects providers (including
local models) via a `.nemesis8.toml` at the project root. Charon plugs in as one
such provider.

### Where the proxy runs
**RECOMMENDED:** consumer proxy runs on the **host**; the sealed container
reaches it at `host.docker.internal`. Grant the container network access to
*only* that endpoint at spawn — the agent can use the remote model and nothing
else (a tight, on-theme sandbox). Alternatively run the proxy as a **sidecar**
in the session's network namespace (`http://charon-proxy:PORT`).

### `.nemesis8.toml` provider block

```toml
[providers.charon]
type     = "openai"
base_url = "http://host.docker.internal:8088/v1"
api_key  = "unused"                 # proxy authenticates upstream via NUTS, not this
models   = ["qwen2.5-coder:32b", "llama3.3:70b"]

[routing]
# Nemesis8 picks provider/model per task by cost/speed/capability;
# the Charon consumer proxy then executes against the chosen trusted provider.
default = "charon"
```

### Launch flow
1. Consumer proxy is running on the host (NUTS token + wallet + pins loaded).
2. `nemesis8` spawns the coding agent (Claude Code, OpenClaw, Cline, Aider, …)
   in a sealed container, network-scoped to the proxy endpoint.
3. The agent calls `host.docker.internal:8088/v1` like any OpenAI server.
4. The proxy resolves → pays → encrypts → relays → decrypts → streams back.
5. Teardown leaves no inbound exposure; session history persists in Nemesis8.

## Model selection is the consumer's job

The gateway cannot route (it cannot read prompts, 00 §3). Selection happens
here, where it belongs: Nemesis8 chooses per-task by cost/speed/capability and
the proxy maps that to a trusted provider. A trivial question SHOULD be sent to
a cheap small model — the consumer controls that, avoiding the
"nickel-per-question to a reasoning model" failure.

## MUST / SHOULD summary
- MUST abort on a pinned-key mismatch (no silent TOFU re-pin).
- MUST present a vanilla OpenAI surface so unmodified agents work.
- SHOULD enforce local budget guards before paying.
- SHOULD bind its listener appropriately (loopback or sidecar network only).
