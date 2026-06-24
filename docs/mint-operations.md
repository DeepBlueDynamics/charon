# Mint operations

The Charon marketplace runs on **one Cashu mint** — the money rail the whole
system shares. **You, the gateway operator, run it once.** Providers and users
do **not** run a mint or a Lightning node.

## Who needs what

| Role | Lightning/mint setup | How they move sats |
|------|----------------------|--------------------|
| **Operator** (you) | Run the mint: `cdk-mintd` + an LND node | This guide. |
| **Provider** (sells models) | None — just `charon keygen` + `charon provider` | Earns Cashu ecash; withdraws by pasting a Lightning invoice (e.g. from Coinbase) → the mint **melts** to it. |
| **User** (buys inference) | None | Pays a Lightning invoice from any wallet (Coinbase) → wallet **mints** ecash, then spends per request. |

Providers and users only ever **pay or receive a Lightning invoice** — Coinbase
handles both. No node, no channels, no liquidity to manage.

## The mint = cdk-mintd + LND

- **LND** — your Lightning node; holds the channels and liquidity. Stateful and
  always-on, so it is **not** a Cloud Run service. Two ways to run it:
  - **Voltage** (`voltage.cloud`) — managed LND, recommended to start. No VM, no
    node ops; they handle uptime + backups.
  - **GCE VM** — self-host LND (Neutrino mainnet) for full sovereignty; more ops.
- **cdk-mintd** — the Cashu mint; issues/redeems ecash and asks LND to create or
  pay invoices.
- Served at **`mint.nuts.services`**. The gateway allowlists it
  (`CASHU_MINT_ALLOWLIST`); the client mints from it (`CASHU_MINT_URL`).

## Stand up the node (Voltage path)

1. Create an **LND node** at voltage.cloud — **mainnet, Standard, AWS**. Name it
   `charon-mint`.
2. **Back up the seed** (print → safe). It is the money; nothing else can
   recover the funds.
3. **Fund on-chain** — open ThunderHub → **On-chain → Receive**, address type
   **P2WKH** (`bc1q…`). Send BTC to it from Coinbase (**Send → Bitcoin**, paste
   the address — pasting an address sends on-chain). Wait for 1 confirmation.
4. **Channels + inbound** — open a small **outbound** channel, and buy
   **inbound** liquidity (Boltz quick-swap / Voltage), so the mint can both pay
   withdrawals and receive deposits.
5. **Grab API creds** — from the Connect / Developer APIs tab: the **gRPC/REST
   endpoint**, the **admin macaroon**, and the **TLS cert**.

> Self-host path: a GCE VM running LND (Neutrino mainnet) + cdk-mintd; static
> channel backups → GCS; macaroon/cert/seed → Secret Manager. Same end state,
> more to operate.

## Why on-chain first?

A fresh node has no channels, so it cannot receive a Lightning payment yet. The
**first** funding is always **on-chain** (send BTC to the node's address). Once
that confirms you open channels — and from then on everything is Lightning.

## Deploy cdk-mintd

1. Put the LND **endpoint + macaroon + cert** in **Secret Manager**.
2. Run `cdk-mintd` with its LND backend pointed at the node — on a small GCE VM,
   or on Cloud Run backed by Cloud SQL (Postgres) for its ecash ledger (its
   state must be durable).
3. Map **`mint.nuts.services`** → cdk-mintd (public TLS).

## Flip Charon to real sats

```bash
# gateway
CASHU_MINT_ALLOWLIST=https://mint.nuts.services
# client (consumer)
CASHU_MINT_URL=https://mint.nuts.services
```

Until this is set, Charon uses the **testnut** test mint
(`https://testnut.cashu.space`) — free fake ecash, so the whole pay/redeem flow
is exercisable end-to-end without real money. Switching the two env vars above
is the only change needed to go live.
