# 12 — UI / Dashboard

A web dashboard for the humans behind the proxies: **log in**, **browse provider
advertisements**, and **fund / settle payments**. It is a control-plane surface
only — it never touches prompts or replies (those are end-to-end encrypted
between the consumer and provider proxies, 04), so the browser, like the gateway,
stays blind to content.

## Who it serves

- **Consumers** — fund a wallet, see prices before spending, curate trusted
  providers (pins, 02/08), set budget guards, review spend and ratings.
- **Providers** — register models + prices, advertise, see earnings and the
  payout address, watch reputation.
- **Both** — one NUTS identity, both roles (a principal can buy and sell).

## Authentication (NUTS browser JWT)

The dashboard uses the **browser JWT** flow, not the daemon `ahp_` flow (02):

1. The user signs in through `auth.nuts.services` (NUTS OIDC); the dashboard
   receives a browser **JWT** (`eyJ…`).
2. The dashboard calls Charon HTTP endpoints with `Authorization: Bearer <jwt>`.
   The gateway validates via `GET {auth}/api/verify` (02) and scopes the
   response to that principal.
3. The principal (email, `sub`) keys the user's directory entry, wallet/ledger,
   pins, and reputation — the same identity used everywhere else (02 triple
   duty).

`ahp_` tokens shown in the dashboard (for running a proxy as a daemon) are
**minted/managed in nuts-auth**, not here; the dashboard MAY deep-link to it.

## Screens

1. **Sign in** — NUTS login; shows the resolved principal.
2. **Marketplace (provider advertisements)** — the directory of currently
   connected providers and their **model cards**: model name, context length,
   `price_msat_per_mtok_in/out` (display in sats), and **settled-sat-weighted
   reputation** (08). Sourced from the gateway directory + reputation reads
   (free, 09) and, optionally, BARKER ads (08). Filter/sort by price, rating,
   model. "Pin provider" adds it to the consumer's trusted set (02).
3. **Wallet / payments** — balance and history; **fund** via:
   - **Lightning** — the dashboard requests a BOLT11 invoice and shows a
     QR/`lightning:` URI; on payment the balance (or a Cashu token) is credited.
   - **Cashu** — paste/redeem a `cashuB` token, or mint from a Lightning deposit
     at an allowlisted mint (recommended for payer privacy, 05).
   Shows the per-request **estimate** (calls `estimate-cost`, 05) so the user
   sees a price before paying.
4. **Provider console** — declare `[[models]]` + prices, toggle advertised,
   view earnings and the **payout address** (BOLT12 / `lno1…`, 06), and ratings.
5. **Consumer console** — manage pins, set per-request/per-session **budget
   guards** (07), review spend and publish ratings (08).

## Gateway HTTP API (control plane)

The relay today speaks WebSocket frames (03). The dashboard needs a small
**read/payment HTTP API** on the gateway (all `Authorization: Bearer <jwt>`):

| Method & path | Purpose |
|---------------|---------|
| `GET /v1/directory` | Connected providers + model cards + prices (09). |
| `GET /v1/providers/{principal}/reputation` | Aggregated signed ratings (08). |
| `POST /v1/quote` | Price a `{model, est_input_tokens, max_tokens}` (05). |
| `POST /v1/wallet/deposit` | Create a Lightning (BOLT11) or Cashu mint deposit. |
| `GET /v1/wallet/balance` | Current msat balance + recent settlements (05). |
| `POST /v1/ratings` | Submit a signed `charon-rating` attestation (08). |

These are control-plane only and reveal nothing about prompt/reply content. The
inference data plane stays on the encrypted WS path (03/04). Reads are subject
to the free-tier rate limits (09).

## Stack (recommended)

A static **single-page app** (e.g. Vite + a lightweight framework), served as
its own container on `dashboard.charon.nuts.services` (or a path on the gateway),
talking only to the gateway HTTP API above and to `auth.nuts.services` for login
— mirroring the nuts.services static-site deploy pattern (11). No server-side
session state; the JWT is the session. Wallet actions are initiated by the
browser but **executed by the gateway/proxy**, which hold the keys — the browser
never holds a spending key or a session key.

## Non-goals

- **No content.** The dashboard never sees or proxies prompts/replies.
- **No custody in the browser.** Lightning/Cashu operations are requested from
  the browser and performed server-side (gateway/proxy wallets, 05/06).
- **No routing.** Model/provider selection is the consumer's job (00 §3, 07);
  the dashboard only surfaces options and records pins.
