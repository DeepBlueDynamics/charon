# Charon documentation

Blind, end-to-end-encrypted LLM inference, paid in bitcoin. The relay never reads your prompts.

| Documentation | Description |
| :--- | :--- |
| [Overview](file:///workspace/charon/docs/README.md) | Introduction to Charon and its architecture |
| [Quickstart](file:///workspace/charon/docs/quickstart.md) | Run the whole marketplace locally in dev mode |
| [Provider guide](file:///workspace/charon/docs/provider-guide.md) | Connect your Ollama models to the marketplace |
| [Consumer guide](file:///workspace/charon/docs/consumer-guide.md) | Configure the OpenAI-compatible proxy and Nemesis8 |
| [Gateway deployment](file:///workspace/charon/docs/gateway-deploy.md) | Deploy the gateway to Google Cloud Run |
| [API reference](file:///workspace/charon/docs/api-reference.md) | Gateway HTTP endpoints and consumer endpoints |
| [Setup checklist](file:///workspace/charon/docs/setup-checklist.md) | Action items and manual setup checklist |

## System architecture

Charon is a blind, paid matchmaking relay between AI consumers (like coding agents) and GPU providers. It solves the privacy and custody issues of centralized LLM providers by ensuring the matching gateway never reads the prompts or completions.

The system consists of three main components:

### Gateway
The gateway (`charon-gateway`) is a blind WebSocket relay and control-plane server. It maintains directories of registered providers, aggregates ratings, quotes fees, verifies payments, and forwards encrypted packets between consumers and providers. **Note:** The gateway cannot read the contents of the prompts or completions. All payload data is end-to-end encrypted.

### Provider
The provider daemon (`charon provider`) runs adjacent to an LLM engine (typically [Ollama](https://ollama.com)). It registers with the gateway, advertises supported models, and processes incoming encrypted inference requests over an outbound-only WebSocket connection. It has no open inbound ports.

### Consumer
The consumer proxy (`charon consumer`) runs locally on the client host or as a sidecar inside a [Nemesis8](file:///workspace/charon/spec/07-consumer-nemesis8.md) container sandbox. It exposes a standard OpenAI-compatible API to local agents, handles payments, establishes end-to-end encrypted [Noise](file:///workspace/charon/spec/04-encryption.md) sessions directly with chosen provider keys, and streams decrypted completions back to the agent.

## Specification documents

For deep implementation details, see the original specification files:
- [00 — Overview](file:///workspace/charon/spec/00-overview.md)
- [01 — Architecture](file:///workspace/charon/spec/01-architecture.md)
- [02 — Identity & Auth](file:///workspace/charon/spec/02-identity-auth.md)
- [03 — Wire Protocol](file:///workspace/charon/spec/03-wire-protocol.md)
- [04 — Encryption](file:///workspace/charon/spec/04-encryption.md)
- [05 — Payments](file:///workspace/charon/spec/05-payments.md)
- [06 — Provider](file:///workspace/charon/spec/06-provider.md)
- [07 — Consumer & Nemesis8](file:///workspace/charon/spec/07-consumer-nemesis8.md)
- [08 — Discovery & Reputation](file:///workspace/charon/spec/08-discovery-reputation.md)
- [09 — Gateway](file:///workspace/charon/spec/09-gateway.md)
- [10 — Security Threat Model](file:///workspace/charon/spec/10-security-threat-model.md)
- [11 — Deployment](file:///workspace/charon/spec/11-deployment.md)
- [12 — UI & Dashboard](file:///workspace/charon/spec/12-ui-dashboard.md)
