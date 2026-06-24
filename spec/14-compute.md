# 14 — Compute (training & GPU rental) — DRAFT / roadmap

**Status:** Draft v0.1 · roadmap stub, not yet implemented. Where this and the
inference specs (03–05) disagree on a shared mechanism, the inference spec wins
until this is promoted out of draft.

Charon's inference path (03–05) is a real-time **blind relay** of small encrypted
request/response calls, settled per call. **Charon Compute** is a phase-2 sibling
for **long-running jobs** — fine-tuning and raw GPU-hour rental — on the *same
provider supply*. It exists because token-selling barely pays back a high-end box
(throughput-capped, commodity token prices), while **hourly compute retains
value** and a discrete fine-tune job is a high-value unit. Adding it is what makes
the 96–128GB+ tier economically rational to recruit, which deepens the premium
supply the inference marketplace also draws on.

## What it reuses

- **Identity & auth (02):** same NUTS principals + per-identity static key.
- **Payment (05):** Cashu/Lightning rails and the gateway fee, but **escrowed by
  job** rather than paid per request (see Escrow).
- **Directory & reputation (08):** the same provider registry + sats-weighted
  reputation; a provider advertises compute capability alongside (or instead of)
  models.
- **Supply base:** the same high-VRAM providers (06) earn from inference *and*
  compute → faster payback → easier to retain.

## What is new

A job is **long-running** (minutes–hours), moves **bulk data**, and returns an
**artifact** — none of which the inference frame protocol (03) handles. New
pieces: a job descriptor, bulk data transfer, checkpointing, artifact return,
escrow settlement, and job-result verification.

## The privacy caveat (read this first)

Inference is blind: a prompt is decrypted only inside the provider's process for
the lifetime of one call. **Compute is NOT blind.** A fine-tune runs the buyer's
**dataset in cleartext** on the provider's machine for hours; the provider can
read it. Consumer hardware (Spark, Apple Silicon, Ryzen AI) has **no TEE /
confidential-compute** to prevent this.

Therefore Charon Compute MUST NOT be marketed with the inference "never sees a
byte" guarantee. It is for **non-sensitive** fine-tunes and batch work. A buyer
with sensitive data SHOULD assume the provider sees it. Confidential compute
(attested TEEs, encrypted-memory GPUs) is a **future** option, not a launch
property — when available it would be advertised as a provider capability and
pinned like a keybind (04).

## Job lifecycle

1. **Submit** — buyer POSTs a **job descriptor** (JSON): kind (`finetune` |
   `batch` | `rental`), base model, hyperparameters, resource requirements
   (VRAM, est. GPU-hours), data location, max price, deadline.
2. **Escrow** — buyer locks payment with the gateway (Cashu) up to `max_price`.
   Nothing releases until acceptance criteria are met (see Escrow).
3. **Match & schedule** — gateway matches the descriptor to a capable provider
   from the directory; provider accepts and reserves the box.
4. **Data transfer** — buyer's dataset is staged to object storage (GCS) the
   provider pulls from; artifacts are written back to the same bucket scheme.
   Data MAY be client-encrypted at rest, but the provider necessarily decrypts
   it to train (see caveat).
5. **Run** — provider executes the job, emitting **progress + checkpoints**
   (step, loss, ETA) the buyer can poll. Providers SHOULD use batching/efficient
   training (e.g. QLoRA) — this is also where multi-tenant inference batching
   would be documented for the inference path.
6. **Return** — provider uploads the artifact (adapter/checkpoint) + a manifest
   (hashes, config, metrics).
7. **Settle** — on acceptance, escrow releases to the provider minus the gateway
   fee; on failure/timeout, escrow refunds per the milestone rules.

## Escrow & pricing

- Pricing is **per GPU-hour** (rental) or **per job** (fine-tune), quoted up
  front; the wire unit stays msat (05).
- Payment is **milestone-escrowed**: e.g. release on accepted checkpoints, or a
  start-deposit + completion-balance. The gateway holds the escrow; refunds on
  provider failure or missed deadline.
- The gateway fee applies to compute as to inference (a configurable cut).

## Verification (open problem)

Inference is roughly self-verifying — the output *is* the value. A training
artifact is **not**: how does a buyer know the provider actually trained honestly
and didn't return a degenerate or copied checkpoint? Candidate mitigations, none
sufficient alone:

- **Manifest + metrics** (loss curve, eval on a held-out probe the buyer
  supplies) — cheap, but spoofable.
- **Spot re-execution** — the buyer (or a third provider) re-runs a small slice
  and checks it matches — expensive, statistical.
- **Reputation stake (08)** — providers stake reputation/sats; fraud burns it.
- **Deterministic seeds + logged steps** — partial reproducibility.

Launch MUST pick an explicit, documented stance (likely reputation-stake +
held-out eval) and SHOULD NOT claim cryptographic verification it can't deliver.

## Non-goals (for now)

- Confidential/blind training (no consumer-HW TEE).
- Cryptographic proof of honest training.
- Distributed multi-node training across providers (single-provider jobs first).

## Open questions

- Escrow primitive: native Cashu, or HTLC/contract on Lightning?
- Data-transfer trust: who pays egress, and is the dataset client-encrypted with
  a key released to the provider on acceptance?
- Does compute capability live in the same directory entry as models (06/08), or
  a separate advertisement?
- Job scheduling/queueing when a provider is mid-job — reservation model.
