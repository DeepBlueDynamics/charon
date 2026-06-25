# Charon Prepaid Credit — buffered, metered billing

**Status:** Draft 1 · **Owner:** Deep Blue Dynamics · **Date:** 2026-06-25
**Scope:** Add an optional prepaid **credit balance** on the gateway so consumers deposit once and requests **meter down at true cost** — eliminating the per-request floor that makes cheap/frequent calls absurdly expensive. Per-request ecash stays as the trustless default; credit is the convenience path.

> This came out of live testing: every buy charged the 21 sat floor while the provider was credited ~0.6 sat. The floor is ~97% overhead on small requests. Prepaid credit amortizes the floor across a deposit and drops the effective take to the markup (~2%).

---

## 1. Problem

The gateway charges `total = max(metered_cost, FLOOR_MSAT)` per request (`FLOOR_MSAT = 21000` = 21 sat, `MARKUP_BPS = 200` = 2%). The split is:

- `provider_msat = metered base` (what the provider earns)
- `gateway_msat = total − provider_msat` (markup **plus** any floor padding)

For a short "say hi" call the base meters to ~0.6 sat, so:

| | msat | note |
|---|---|---|
| consumer pays | 21 sat | the floor |
| provider credited | ~0.6 sat | true metered value |
| gateway keeps | ~20.4 sat | floor padding (~97% take) |

The floor exists to cover **per-request overhead** — chiefly that every request today redeems a fresh Cashu token (mint round-trip + swap fees), which isn't worth doing for sub-sat value. But it makes Charon unusable for cheap models and high request rates, which is exactly the Ollama-cloud-reseller use case.

Observed in the wallet ledger (one principal acting as both sides nets −20.4 sat/request):
```
… settlement -21 sat   (consumer pays floor)
… cashu_credit 0.617 sat (provider earns metered)
```

---

## 2. Fix: prepaid credit, meter down at cost

Consumer **deposits a chunk once** (e.g. 2000 sat) → held as **credit on the gateway** → each request **deducts the true metered cost, no per-request floor** → gateway earns its markup on real usage.

| | per-request ecash (today) | prepaid credit (new) |
|---|---|---|
| 2000 sat buys | ~95 calls (floor-bound) | ~3,000+ calls (metered) |
| effective gateway take | up to ~97% on tiny calls | ~2% (markup) |
| Cashu redemptions | one per request | one per deposit |
| trust | blind / non-custodial | custodial credit (see §6) |

**Both paths coexist.** The consumer chooses: trustless per-request ecash for privacy/zero-custody, or prepaid credit for cheap-and-fast. Default stays per-request; credit is opt-in.

---

## 3. Data model

The gateway already has per-principal balances in Firestore (`wallets/{principal}`, `get_balance`, `record_wallet_event`, the dashboard ledger). Extend, don't replace.

**Unified balance per principal** (`balance_msat`) with a typed ledger. Event kinds:

| kind | sign | meaning |
|---|---|---|
| `deposit` | + | consumer redeemed ecash into credit |
| `debit` | − | a request metered against credit |
| `cashu_credit` | + | provider earnings from a settlement (exists) |
| `cashu_fee` | + (to `gateway`) | gateway markup (exists) |
| `payout` | − | provider melted earnings to LN (future, spec 06) |

Net `balance_msat` = deposits + earnings − debits − payouts. A principal that is both consumer and provider has one balance; **earnings can fund spends** (a provider can pay for inference out of what it earned — desirable). Keep the option to split consumer-vs-provider sub-balances later if abuse requires it.

Custody pool: the gateway operates **one custodial Cashu wallet** ("the pool"). Deposits and per-request payments are *redeemed* into the pool; provider payouts melt out of it. `balance_msat` is the accounting claim against the pool. (Today the gateway only *verifies* payment — §5.1 makes it *redeem*.)

---

## 4. Flows

### 4.1 Deposit (consumer → credit)
1. Consumer mints one large ecash token from its local cdk wallet (existing `spend_cashu_token`, larger amount).
2. New gateway endpoint **`POST /v1/credit/deposit`** (authed) with the token, OR a new `Frame::Deposit { token }` over the relay WS.
3. Gateway redeems the token into the pool, `record_wallet_event(principal, "deposit", +amount, "settled")`.
4. Cap deposit/balance at a configurable ceiling (default ~$10 ≈ 16k sat) to bound custodial exposure.

### 4.2 Metered request (debit from credit)
In `Frame::Open` (gateway `lib.rs`):
1. Compute `metered = quote(rate, tokens, markup, floor=0)` — **floor disabled on the credit path**.
2. If `get_balance(consumer) ≥ metered.total_msat`:
   - `record_wallet_event(consumer, "debit", −total, "settled")`
   - credit provider + gateway exactly as today (`cashu_credit`, `cashu_fee`)
   - proceed — **no `Payment::Cashu` required in the envelope, no floor**
3. Else: fall back to the current per-request ecash path (with floor), or return `402` with "insufficient credit".

### 4.3 Top-up
Same as deposit; the consumer page surfaces a "credit balance" + "Add credit" alongside the existing wallet QR fund flow.

---

## 5. Code touch-points

### 5.1 Gateway (`crates/gateway/src/lib.rs`)
- `Frame::Open` (~1663): branch on consumer credit before the payment-verify/floor path (§4.2). Reuse `state.get_balance` / `record_wallet_event`.
- New `Frame::Deposit` (or HTTP `POST /v1/credit/deposit`) → redeem ecash into the pool + credit (§4.1).
- Pool wallet: a custodial cdk wallet on the gateway (it already pulls `cdk`); deposits/payments redeem into it. Gate behind config so the trustless-only deploy can disable credit.
- `quote()` call gains a `floor_override = 0` for credit-paid requests (charon-core `payment::quote` already takes `floor_msat`).

### 5.2 Consumer (`crates/client/src/main.rs`)
- New deposit command/endpoint: mint a large token (`spend_cashu_token`) → `POST /v1/credit/deposit`.
- `chat_completions` / `consumer_relay`: if credit mode is on, **skip `spend_cashu_token`** and send an `Open` without `Payment::Cashu`; the gateway debits credit.
- Consumer console (`CONSUMER_HOME`): show gateway credit balance + "Add credit" (amount → deposit), distinct from the local-wallet QR top-up.

### 5.3 Wire (`crates/core/src/wire.rs`)
- `Payment` enum gains a `Credit` (or `None`) variant for credit-paid opens; or make `payment` optional in `Envelope` when the consumer has credit.

---

## 6. Trust & custody (the real tradeoff)

Prepaid credit makes the gateway **custodial** for the deposited balance — the inverse of Charon's per-request blind ecash, where the gateway never holds consumer funds. This is a deliberate convenience/trust swap:

- **Bounded:** cap balances low (default $10). A bad gateway can steal at most the cap.
- **Optional:** trustless per-request ecash remains the default; credit is opt-in per consumer.
- **Reputation-backed:** providers (and the gateway) carry reputation tied to an account/identity. A provider that under-delivers against prepaid credit loses standing; burner identities start at zero reputation (handled in spec 08 discovery/reputation — a burner is just a new account).
- **Not a mint:** the pool is an operational float, not issued money; balances are off-chain accounting claims, redeemable as inference, meltable to LN by providers.

State this explicitly in the consumer console so users opt in with eyes open.

---

## 7. Bonus: absorbs the refund gap

The current per-request path charges (mints + sends ecash) **before** the relay confirms delivery; a `ProviderGone`/failed relay eats the ecash with no refund (a known gap). On the credit path there is **nothing to refund** — the gateway simply doesn't `debit` if delivery fails. Order the Open handler so the `debit` is recorded only **after** a successful relay/settlement (or reverse it on failure). This is strictly better than minting-then-hoping.

---

## 8. Out of scope (future)

- Provider payout/melt from the pool to LN (spec 06 follow-up).
- Per-consumer-vs-provider sub-balances (only if abuse needs it).
- Streaming/usage-true metering beyond `max_tokens` estimate.
- Multi-gateway credit portability.

---

## 9. Implementation plan

1. **Gateway redeem + deposit** — pool wallet, `Frame::Deposit`/`POST /v1/credit/deposit`, `deposit` ledger event. (Behind a `CHARON_ENABLE_CREDIT` flag.)
2. **Open meter-down** — credit branch in `Frame::Open`, floor disabled, debit after relay success (§4.2 + §7).
3. **Consumer** — deposit flow + credit-mode opens (skip per-request mint).
4. **Consumer console** — credit balance + "Add credit"; consumer chooses credit vs per-request.
5. **Caps + copy** — balance ceiling, custodial-disclosure text.

## 10. Verification

- Deposit 2000 sat → gateway credit shows 2000; pool balance up by ~2000.
- Run 50 small buys on credit → each debits true metered cost (sub-sat to a few sat), **no 21 sat floor**; provider earns metered; gateway earns ~2%; credit decremented by the sum.
- Kill the provider mid-buy → credit **not** debited (refund gap closed).
- Per-request ecash path still works unchanged when credit is off/empty.
- Balance cap rejects deposits over the ceiling.
