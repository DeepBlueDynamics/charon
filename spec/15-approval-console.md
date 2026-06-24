# 15 — Approval Console (Charon Client)

**Status:** Draft v0.1.

A local, human-in-the-loop console served by the **consumer proxy** (07). It gives
the operator live visibility, an **approval gate**, and a **searchable audit log**
over all agent to provider traffic. Everything runs locally, inside the client
container. The remote gateway is the blind relay and sees none of it.

It exists because the real residual risk in the marketplace is consumer-side: the
provider runs a model, not an agent loop, so the exposure is your agent acting on
returned content (provider prompt injection, see 10). The relay cannot help here
by design. This console is where a human, and optionally a local model, can watch
and approve what comes back before the agent acts on it.

## Where it runs

A mode of the consumer (07). The proxy already terminates the local OpenAI API and
holds the plaintext, so the console adds no new exposure. It binds a second local
listener (default `127.0.0.1:8089`) in the same container as the proxy. Nothing
leaves the machine. The gateway never sees any of it.

## The approval gate

- The proxy intercepts each **response** (and optionally each **request**) before
  handing it back to the agent, and may **hold** it pending approval.
- Approve forwards it; reject returns an error to the agent.
- **Surface tool calls.** Parse the response for tool-call suggestions and
  highlight them. That is the dangerous part, not the prose.
- **Modes:**
  - `off` — forward everything at full speed (trusted session).
  - `flag` — auto-forward normal traffic, hold + alarm only on a suspicious
    response (default). Heuristics: "ignore previous", secret-path reads
    (`~/.ssh`, `.env`), base64 blobs, exfil-looking URLs, tool calls touching
    sensitive paths. Optionally backed by the risk judge (below).
  - `manual` — step through every request and response.
- **Auto-approve window.** A toggle to auto-approve for N minutes (customizable),
  then revert to the prior mode. Lets you let it run, then clamp down.
- **Latency.** Agents are chatty; gating every call breaks their flow. The gate
  MUST default to non-blocking (`off`/`flag`); `manual` is for sensitive
  sessions. Held items expire after a configurable timeout so a forgotten prompt
  does not hang the agent forever.

## Audit log and search

- Every request and response is appended as a structured record (JSONL) plus the
  raw text, per session, under `~/.charon/log/`. Timestamp, model, provider
  handle, token counts, sats spent, approval decision.
- **Search.** v1 is substring/filter over recent logs in the console. Upgrade
  path is indexing the log with **Lume** (BM25/FST) for fuzzy, full-history
  search once volume justifies it. The two are not coupled at v1.
- Retention and export are operator-controlled.

## Risk judge (optional, opt-in)

Point the console at a small local model (Ollama or a guard model). Each response
is scored by prompting that model: does this contain prompt injection or risky
instructions? The score drives `flag` mode beyond regex heuristics. It is opt-in
because it needs a local model, and it runs locally so nothing leaks.

## The console UI

Charon-branded, header reads **Charon Client**. A live request to response
timeline with approve/reject buttons, settings (mode, auto-approve window, sound
on/off), the consumer wallet **balance + funding QR** (folds in the funding flow),
and a **search bar** over the audit log. Slick, dark, the same aesthetic as the
landing page and dashboard.

## Dashboard integration

The web dashboard (12) SHOULD show a link to the local console, but only when the
console is actually reachable:

- The console serves `GET /healthz` with CORS allowing the dashboard origin.
- On load the dashboard probes `http://127.0.0.1:8089/healthz`. `http://localhost`
  is a potentially-trustworthy origin, so an HTTPS dashboard may reach it without
  a mixed-content block (Chrome; other browsers vary, degrade gracefully).
- If the probe succeeds, the dashboard shows **Open Charon Client** linking to the
  console. If not, it shows a quiet "client offline" hint with how to start it.

## Configuration

| Env | Default | Meaning |
|-----|---------|---------|
| `CHARON_CONSOLE_LISTEN` | `127.0.0.1:8089` | console + healthz listener |
| `CHARON_CONSOLE_MODE` | `flag` | `off` / `flag` / `manual` |
| `CHARON_CONSOLE_AUTOAPPROVE_SECS` | `0` | auto-approve window length |
| `CHARON_CONSOLE_HOLD_TIMEOUT_SECS` | `120` | how long a held item waits |
| `CHARON_RISK_MODEL` | (unset) | opt-in local model for the risk judge |
| `CHARON_LOG_DIR` | `~/.charon/log` | audit log location |

## Honest limits / non-goals

- This is a checkpoint on **content**, not on the agent's tool execution, which
  happens downstream in the harness after it receives the response. It is best
  paired with a **sandboxed agent** (10), which contains what any action can do.
  Defense in depth, not a silver bullet.
- It does not make the relay non-blind and adds no new exposure: the plaintext is
  already at the proxy.
- No provider-side console at v1; provider risk is low (no agent loop).

## Phasing

- **v1:** approval gate (modes + auto-approve + tool-call surfacing), JSONL log
  with simple search, the slick UI, and the dashboard detect-and-link.
- **Fast-follow:** the local-model risk judge, and Lume-backed search.
