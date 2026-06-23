# 13 — MCP Server

Charon exposes a local **MCP server** so any MCP-capable agent (Claude Code,
Cline, …) can buy inference as tools — and when a call needs paying, the server
answers with a **402-style `payment_required`** result that carries a Lightning
invoice rendered as a **scannable ASCII QR**. The human scans it from a phone
wallet (Coinbase, Phoenix, …), pays, and the agent retries. It is the L402
challenge, expressed MCP-native, with a human-in-the-loop pay step that works in
a terminal with no browser.

## Where it runs

The MCP server is a mode of the **client**: `charon mcp` (stdio transport),
wrapping the same machinery as `charon consumer` (07) — NUTS identity, wallet,
trusted-provider pins, model→provider resolution. It speaks to the gateway over
the same encrypted WS path (03/04); prompts stay end-to-end encrypted, so the
MCP layer never weakens the blind relay. The agent config points at it like any
stdio MCP server:

```json
{ "mcpServers": { "charon": { "command": "charon", "args": ["mcp"] } } }
```

## Tools

| Tool | Input | Result |
|------|-------|--------|
| `list_models` | — | resolvable models + sat price per Mtok (in/out) |
| `estimate_cost` | `{model, messages?, max_tokens}` | `{est_input_tokens, max_tokens, total_sat, total_msat}` (05) |
| `chat` | `{model, messages, max_tokens?}` | the completion **or** a `payment_required` result (below) |
| `invoice_qr` | `{invoice}` | `{lightning_uri, qr_ascii}` — render any BOLT11 as a QR |
| `balance` | — | `{balance_sat, balance_msat}` (consumer wallet) |

`chat` is the main path. If the wallet can cover the quote it pays and returns
the completion. If it can't, it returns `payment_required` instead of erroring.

## The `payment_required` result (the "402")

A structured, recognizable object an agent harness can branch on:

```json
{
  "status": "payment_required",
  "http_status": 402,
  "rail": "lightning",
  "amount_sat": 250,
  "amount_msat": 250000,
  "invoice": "lnbc2500n1p…",
  "lightning_uri": "lightning:lnbc2500n1p…",
  "qr_ascii": "█▀▀▀▀▀█ ▀▄█ █▀▀▀▀▀█\n█ ███ █ …",
  "payment_hash": "…",
  "expires_at": 1750000000,
  "retry": { "tool": "chat", "args_echo": { "model": "…", "max_tokens": 0 } }
}
```

- `qr_ascii` is the **invoice as a text QR** (see below) — the headline field:
  the agent prints it, the human scans and pays.
- `retry` tells the agent how to re-issue the same call once paid. The server
  correlates by `payment_hash` (Lightning) / change token (Cashu), so a plain
  re-call of `chat` after payment succeeds; `payment_hash` lets the agent poll.
- For **Cashu**, `rail:"cashu"` and the invoice is the *mint's* funding BOLT11
  (pay it → the consumer wallet mints ecash → the next `chat` pays from it). The
  QR is still a `lightning:` invoice, so the scan-to-pay UX is identical.

The MCP server SHOULD also emit a human-readable text block alongside the
structured result (the ASCII QR + "scan to pay 250 sat") so agents that just
print tool output show something useful with no special handling.

## ASCII QR rendering

`qr_ascii` encodes the `lightning:` URI as a QR using a Rust QR crate (e.g.
`qrcode`) rendered with **Unicode half-blocks** (`▀ ▄ █`, two rows per line) so
it is compact and scannable in a terminal; a pure-ASCII (`██`/spaces) fallback
is provided for terminals without Unicode. Quiet-zone padding is included so
phone cameras lock on. The same renderer backs the `invoice_qr` tool.

## Flow

1. Agent calls `chat {model, messages, max_tokens}`.
2. Wallet can't cover it → server returns `payment_required` with `qr_ascii`.
3. Agent prints the QR; the human scans it and pays from any wallet.
4. Agent re-calls `chat` (same args). The server sees the invoice settled (or
   the wallet now funded) and runs the paid request → returns the completion.
5. Subsequent calls draw from the funded balance until it runs low, then the
   cycle repeats. `estimate_cost` lets an agent show the price before step 1.

## Notes

- The MCP server holds the consumer's keys/wallet **locally**; the gateway and
  the QR/invoice reveal no prompt content (04). An invoice is public by nature.
- Budget guards (07) apply here too: a per-call / per-session sat ceiling the
  server enforces before paying or before emitting an invoice.
- This composes with Nemesis8 (07): run `charon mcp` as the sidecar and wire it
  into the sealed agent as an MCP server instead of (or alongside) the OpenAI
  endpoint.
