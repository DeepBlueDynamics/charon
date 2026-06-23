# Charon — Your Setup Checklist

The action items that need **you** (Kord), not the agents — decisions only you can
make, and external accounts/DNS/secrets the code can't provision itself. Ordered
roughly by what unblocks the most. Hand the decisions back to me and I'll wire
them; the rest are commands for you to run (some need interactive login).

For step-by-step usage of each role, see the guides in this folder
(`provider-guide.md`, `consumer-guide.md`, `gateway-deploy.md`).

---

## 1. Decisions I need from you (these unblock code)

- [ ] **NUTS keybind signing primitive.** What does a NUTS identity sign the
      X25519 key binding with — an LNURL-auth-style linking key, a key registered
      in nuts-auth, or a Nostr key? Until this is fixed, `crypto::verify_keybind`
      checks key/expiry/principal but **not** the final signature, so first-contact
      MITM (threat T2 in spec 10) isn't fully closed. Tell me the primitive →
      I wire real verification.
- [ ] **Primary payment rail.** Cashu (recommended, payer-blind), L402/Lightning,
      and prepaid balance are all specced (05). Which do we implement first?
      Today the gateway runs a dev-accept stub.
- [ ] **Cashu mint allowlist.** If Cashu: give me the mint URL(s) the gateway may
      swap tokens at (`CASHU_MINT_ALLOWLIST`).
- [ ] **Gateway fee.** Defaults are `MARKUP_BPS=1000` (+10%) and
      `FLOOR_MSAT=21000` (21 sat). Confirm or change.

## 2. Google Cloud (to deploy the gateway)

The gateway deploys to Cloud Run exactly like the other nuts.services Rust
services (project `gnosis-459403`, region `us-central1`).

- [ ] Confirm you have access to project **`gnosis-459403`** and it has billing.
- [ ] Authenticate (interactive — run it yourself in this session):
      `! gcloud auth login` then `! gcloud config set project gnosis-459403`
- [ ] Enable APIs (one-time):
      `! gcloud services enable run.googleapis.com cloudbuild.googleapis.com artifactregistry.googleapis.com`
- [ ] Decide: do you want **me to run the deploy** once you've authenticated, or
      will you run the `gcloud run deploy` command? (I'll have it ready in
      `gateway-deploy.md`.)

## 3. DNS (public endpoint)

- [ ] After the Cloud Run **domain mapping** for `charon-gateway` is created
      (I can do that step), add this DNS record where `nuts.services` is managed:
      `CNAME  charon  ->  ghs.googlehosted.com.`
      Google then auto-provisions TLS (15–30 min). Public endpoint becomes
      `wss://charon.nuts.services/ws`.

## 4. NUTS identity & tokens

- [ ] Each **provider** and **consumer** needs an `ahp_` API token from
      `auth.nuts.services` (the NUTS dashboard → API tokens). Mint one per daemon.
- [ ] For your own testing: mint at least one `ahp_` token and have it ready as
      `NUTS_AHP_TOKEN`.
- [ ] (Production) Confirm the gateway should validate against
      `https://auth.nuts.services` (default) and that **`DISABLE_AUTH` is never
      set** on the public gateway.

## 5. Secrets (production)

- [ ] Decide where wallet/mint credentials live. Pattern is GCP **Secret Manager**
      mounted to the Cloud Run service account (`roles/secretmanager.secretAccessor`)
      — never baked into the image or passed as plain env vars. Tell me which
      secrets exist once payments are real (mint key, payout wallet creds).

## 6. Try it locally first (no cloud needed)

Before any of the above, you can run the whole thing on your machine in dev mode
(`DISABLE_AUTH=true`). Steps are in `gateway-deploy.md` (dev section),
`provider-guide.md`, and `consumer-guide.md`. Quick version:

```bash
DISABLE_AUTH=true cargo run -p charon-gateway              # :8080
cargo run -p charon -- provider --ollama http://localhost:11434
cargo run -p charon -- consumer --listen 127.0.0.1:8088
# then point any OpenAI client at http://127.0.0.1:8088/v1
```

---

### Hand back to me
The fastest unblock: **(1) the keybind primitive**, **(1) the payment rail +
mint URL**, and **(2) confirmation you can `gcloud auth login` to
`gnosis-459403`**. With those three I can close the MITM gap, start real
payments, and deploy.
