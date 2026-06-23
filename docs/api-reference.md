# API reference

API endpoints exposed by the Charon gateway control plane and the local consumer proxy.

## Gateway endpoints

All gateway endpoints are served on port 8080. In production, requests require a Bearer token in the `Authorization` header. In dev mode (`DISABLE_AUTH=true`), this validation is bypassed.

### Get directory
Retrieve the list of registered providers and their model cards.

- **Method:** `GET`
- **Path:** `/v1/directory`
- **Auth:** Bearer Token

#### Request fields
No request body or parameters required.

#### Response fields
| Field | Type | Required | Description |
| :--- | :--- | :--- | :--- |
| `principal` | `String` | Yes | The NUTS identity of the provider. |
| `models` | `Array` | Yes | List of model cards offered by the provider. |
| `models[].name` | `String` | Yes | Name of the model. |
| `models[].backend` | `String` | Yes | Underlying engine (e.g. `ollama`). |
| `models[].context_length` | `u32` | Yes | Supported token context length. |
| `models[].price_msat_per_mtok_in` | `u64` | Yes | Input token price in millisats per million tokens. |
| `models[].price_msat_per_mtok_out` | `u64` | Yes | Output token price in millisats per million tokens. |

#### Example

```bash
# Fetch the gateway directory
curl -H "Authorization: Bearer my_nuts_token" http://localhost:8080/v1/directory
#
# Response (200 OK):
# [
#   {
#     "principal": "provider_a",
#     "models": [
#       {
#         "name": "qwen2.5-coder:32b",
#         "backend": "ollama",
#         "context_length": 4096,
#         "price_msat_per_mtok_in": 200000,
#         "price_msat_per_mtok_out": 600000
#       }
#     ]
#   }
# ]
```

### Get reputation
Retrieve performance metrics and historical scores for a provider.

- **Method:** `GET`
- **Path:** `/v1/providers/{principal}/reputation`
- **Auth:** Bearer Token

#### Request fields
| Field | Type | Required | Description |
| :--- | :--- | :--- | :--- |
| `principal` | `String` | Yes | Path parameter. NUTS identity of the provider. |

#### Response fields
| Field | Type | Required | Description |
| :--- | :--- | :--- | :--- |
| `ratings` | `Array` | Yes | List of feedback rating objects (stubbed empty). |
| `average_score` | `f64` | Yes | The provider's average rating score (stubbed to `0.0`). |
| `total_settled_msat` | `u64` | Yes | Total volume in millisats processed through this provider (stubbed to `0`). |

#### Example

```bash
# Fetch provider reputation
curl -H "Authorization: Bearer my_nuts_token" http://localhost:8080/v1/providers/provider_a/reputation
#
# Response (200 OK):
# {
#   "ratings": [],
#   "average_score": 0.0,
#   "total_settled_msat": 0
# }
```

### Post quote
Calculate the billing breakdown (provider share, gateway fee, total) for a query.

- **Method:** `POST`
- **Path:** `/v1/quote`
- **Auth:** Bearer Token

#### Request fields
| Field | Type | Required | Description |
| :--- | :--- | :--- | :--- |
| `model` | `String` | Yes | Name of the model. |
| `est_input_tokens` | `u32` | Yes | Consumer's estimated input token count. |
| `max_tokens` | `u32` | Yes | Maximum output tokens cap. |

#### Response fields
| Field | Type | Required | Description |
| :--- | :--- | :--- | :--- |
| `provider_msat` | `u64` | Yes | Cost share going directly to the provider in millisats. |
| `gateway_msat` | `u64` | Yes | Gateway commission in millisats. |
| `total_msat` | `u64` | Yes | Total billing cost in millisats. |

#### Example

```bash
# Request a price quote
curl -X POST http://localhost:8080/v1/quote \
  -H "Authorization: Bearer my_nuts_token" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "qwen2.5-coder:32b",
    "est_input_tokens": 100,
    "max_tokens": 500
  }'
#
# Response (200 OK):
# {
#   "provider_msat": 320000,
#   "gateway_msat": 32000,
#   "total_msat": 352000
# }
```

### Wallet and feedback stubs
These endpoints represent the spec-12 financial and ratings controls. The gateway codebase currently contains placeholder stubs that return a `501 Not Implemented` status.

#### Post deposit
Deposit bitcoin into the gateway wallet.
- **Method:** `POST`
- **Path:** `/v1/wallet/deposit`
- **Auth:** Bearer Token
- **Response:** `501 Not Implemented` ("TODO: POST /v1/wallet/deposit (spec 12)")

#### Get balance
Retrieve the wallet balance.
- **Method:** `GET`
- **Path:** `/v1/wallet/balance`
- **Auth:** Bearer Token
- **Response:** `501 Not Implemented` ("TODO: GET /v1/wallet/balance (spec 12)")

#### Post ratings
Submit feedback for a model session.
- **Method:** `POST`
- **Path:** `/v1/ratings`
- **Auth:** Bearer Token
- **Response:** `501 Not Implemented` ("TODO: POST /v1/ratings (spec 12)")


## Consumer endpoints

The consumer proxy exposes an OpenAI-compatible API locally on port 8088. Authentication is handled by NUTS between the proxy and gateway, so agents do not need to supply real API keys.

### List models
List the pinned provider models available for local routing.

- **Method:** `GET`
- **Path:** `/v1/models`
- **Auth:** None (ignored)

#### Response fields
| Field | Type | Required | Description |
| :--- | :--- | :--- | :--- |
| `object` | `String` | Yes | Response type metadata. Always `list`. |
| `data` | `Array` | Yes | List of model descriptors. |
| `data[].id` | `String` | Yes | Model identifier. |
| `data[].object` | `String` | Yes | Object descriptor. Always `model`. |
| `data[].owned_by` | `String` | Yes | The provider NUTS principal serving this model. |

#### Example

```bash
# List available consumer models
curl http://localhost:8088/v1/models
#
# Response (200 OK):
# {
#   "object": "list",
#   "data": [
#     {
#       "id": "qwen2.5-coder:32b",
#       "object": "model",
#       "owned_by": "provider_a"
#     }
#   ]
# }
```

### Estimate cost
Estimate the exact cost breakdown for a chat request.

- **Method:** `POST`
- **Path:** `/v1/estimate-cost`
- **Auth:** None

#### Request fields
| Field | Type | Required | Description |
| :--- | :--- | :--- | :--- |
| `model` | `String` | Yes | Model identifier. |
| `messages` | `Array` | No | Chat prompt message array. |
| `max_tokens` | `u32` | No | Maximum token cap. Defaults to `1024`. |

#### Response fields
| Field | Type | Required | Description |
| :--- | :--- | :--- | :--- |
| `model` | `String` | Yes | Selected model. |
| `provider` | `String` | Yes | Provider principal serving the model. |
| `est_input_tokens` | `u32` | Yes | Estimated input tokens computed from the message array. |
| `max_tokens` | `u32` | Yes | Session max output token limit. |
| `provider_msat` | `u64` | Yes | Provider share in millisats. |
| `gateway_msat` | `u64` | Yes | Gateway commission in millisats. |
| `total_msat` | `u64` | Yes | Total billing cost in millisats. |

#### Example

```bash
# Request cost preview
curl -X POST http://localhost:8088/v1/estimate-cost \
  -H "Content-Type: application/json" \
  -d '{
    "model": "qwen2.5-coder:32b",
    "messages": [{"role": "user", "content": "Hello"}],
    "max_tokens": 100
  }'
#
# Response (200 OK):
# {
#   "model": "qwen2.5-coder:32b",
#   "provider": "provider_a",
#   "est_input_tokens": 8,
#   "max_tokens": 100,
#   "provider_msat": 76000,
#   "gateway_msat": 21000,
#   "total_msat": 97000
# }
```

### Chat completions
Exposes the OpenAI standard chat endpoint. Supports JSON and SSE streaming.

- **Method:** `POST`
- **Path:** `/v1/chat/completions`
- **Auth:** None

#### Request fields
| Field | Type | Required | Description |
| :--- | :--- | :--- | :--- |
| `model` | `String` | Yes | Model identifier. |
| `messages` | `Array` | Yes | OpenAI message list (e.g. `[{"role": "user", "content": "hi"}]`). |
| `stream` | `Boolean` | No | Enables SSE stream responses. Defaults to `false`. |
| `max_tokens` | `u32` | No | Output token ceiling. |

#### Response fields
Returns standard OpenAI chat completions or SSE chunks.

#### Example

```bash
# Query chat completion (buffered)
curl -X POST http://localhost:8088/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "qwen2.5-coder:32b",
    "messages": [{"role": "user", "content": "Say hello!"}],
    "stream": false
  }'
#
# Response (200 OK):
# {
#   "id": "chatcmpl-a96d19ef-505f-45a8-b649-165f972b9a71",
#   "object": "chat.completion",
#   "created": 1719168400,
#   "model": "qwen2.5-coder:32b",
#   "choices": [
#     {
#       "index": 0,
#       "message": {
#         "role": "assistant",
#         "content": "charon dev provider response"
#       },
#       "finish_reason": "stop"
#     }
#   ],
#   "usage": {
#     "prompt_tokens": 8,
#     "completion_tokens": 4,
#     "total_tokens": 12
#   }
# }
```
