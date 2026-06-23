# 00 — Overview

## What this is

Charon is a marketplace where independent **providers** sell LLM inference from
hardware they control, and **consumers** buy it per request, paid in bitcoin.
A central **gateway** introduces the two parties, relays their traffic, and
settles payment — but the prompt and the reply are encrypted end-to-end between
consumer and provider, so the gateway is a *blind* matchmaker: it can prove it
got paid and can prove nothing else about the content.

It is built to compose with the NUTS / DeepBlue Dynamics ecosystem: identity
comes from **nuts-auth**, providers serve models via **Ollama**, consumers
drive agents via **Nemesis8**, and discovery MAY use **BARKER**.

## Actors

- **Provider** — runs Ollama plus a *provider proxy*. Chooses which local models
  to sell and at what price. Holds a NUTS identity and a wallet for payouts.
- **Consumer** — runs a *consumer proxy* that exposes an OpenAI-compatible API
  locally. Holds a NUTS identity and a wallet. Drives any agent (via Nemesis8)
  that speaks the OpenAI API.
- **Gateway** (a.k.a. the Relay) — a hosted service that authenticates both
  proxies by NUTS token, matches a consumer to a provider, relays opaque
  frames, settles payment, takes a fee, and aggregates reputation.

## Core principles

1. **Blind relay.** The gateway routes *everything*, but the request and
   response bodies are end-to-end encrypted. The gateway never holds a session
   key and never sees plaintext. (See 04.)
2. **End-to-end means consumer↔provider.** "Encrypted" protects content from
   the gateway and the network — not from the provider, which must decrypt to
   run the model. Provider-blindness is out of scope (see 10 for the TEE path).
3. **No server-side routing.** The gateway cannot read prompts, so it cannot
   classify or route them. Model selection is the **consumer's** job. This is a
   feature: it also kills the failure mode where a trivial question gets routed
   to an expensive reasoning model.
4. **Identity is one object, three jobs.** A party's NUTS identity gates its
   connection, anchors its end-to-end encryption key, and accrues its
   reputation. (See 02, 08.)
5. **Trust is relational and client-side.** Consumers curate trusted providers
   (pinned keys, ratings). The marketplace surfaces signed reputation but does
   not adjudicate quality.
6. **Reputation is portable.** Ratings are signed attestations anchored to a
   provider's identity, weighted by sats actually transacted, and publishable
   outside any single gateway — so a fork does not reset them. (See 08.)
7. **Fork-friendly.** The gateway is small and self-hostable. Any provider /
   consumer cluster can run its own. The public gateway is a default rendezvous,
   not a chokehold.

## Explicit non-goals

- **Hiding content from the provider.** Out of scope without confidential
  compute. See 10.
- **Hiding traffic *shape*.** Per-chunk sizes and timing are observable by the
  relay. This is an accepted leak. Optional padding is described in 04 but not
  required.
- **Server-side prompt routing / classification.** Removed by design (principle 3).
- **A custodial bank.** Where the gateway touches funds it does so transiently
  to settle; minimizing custody is a goal (see 05, 10).

## Glossary

- **NUTS token** — credential from `auth.nuts.services`: an `ahp_` API token or
  a browser JWT. Identifies a principal by email. (See 02.)
- **Provider proxy** — the process a provider runs next to Ollama.
- **Consumer proxy** — the local OpenAI-compatible endpoint a consumer runs.
- **Envelope** — the small cleartext control object the gateway reads to route
  and price a request. (See 03.)
- **Payload** — the AEAD-encrypted request/response body the gateway cannot read.
- **Pin** — a consumer's stored, identity-bound static key for a trusted provider.
- **Settled sats** — payment actually cleared through the gateway for a session;
  the unit reputation is weighted by.
