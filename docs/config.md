# Configuration

rua reads its configuration from `~/.config/rua/config.toml`. The file is
auto-created with sensible defaults on first run if it does not exist.

## Config path

```
~/.config/rua/config.toml
```

You can also inspect the resolved path at runtime via
`rua_engine::config::config_path()`.

## Top-level `model` — the current model

```toml
model = "deepseek/deepseek-v4-pro"
```

The current model is a *model ref*: `"provider/model"`, pointing at one of
the `[[providers]]` entries below (validated at startup — unknown provider
or a bare model name is a loud config error). Requests that don't override
the model (UI default, summarize, spawn) use it.

## `[[providers]]` — the provider registry

Providers are a flat named list; there is no implicit default provider.

| Key                 | Default                        | Description                                  |
|---------------------|--------------------------------|----------------------------------------------|
| `name`              | —                              | Unique name used in model refs; must not contain `/` |
| `kind`              | `"openai"`                     | `"openai"` = OpenAI-compatible endpoint (incl. LM Studio); `"deepseek"` = DeepSeek API |
| `api_key`           | `""`                           | API key (see formats below)                  |
| `base_url`          | `"http://127.0.0.1:1234/v1"`   | API base URL                                 |
| `models`            | `[]`                           | Optional static model list, merged into the UI picker (see below) |
| `additional_params` | `{}`                           | Provider-specific request params, passed through verbatim |
| `connect_timeout_secs` | `10`                        | TCP connect timeout                          |
| `read_timeout_secs` | `120`                          | Idle gap between stream reads; `0` disables  |
| `llm_max_retries`   | `3`                            | Retries for calls that produced no content yet |
| `llm_retry_base_ms` | `1000`                         | Exponential backoff base, capped at 60s      |

```toml
[[providers]]
name = "deepseek"
kind = "deepseek"
api_key = "$DEEPSEEK_API_KEY"
base_url = "https://api.deepseek.com"
```

### Model lists and `/api/models`

The daemon's `GET /api/models` builds each provider's list as: the static
`models` entries first (curated, kept in order), then the dynamic
`GET {base_url}/models` result deduplicated after them (fetched in
parallel, 2s timeout + 60s cache per provider; on failure the last cache or
the static list is all that shows). Every returned entry's `id` is the full
model ref — the UI model picker sends it back as the per-send override.

### `api_key` value formats

The `api_key` field supports three value formats, resolved at startup in the
following order:

1. **Shell command** — prefix with `!`. The rest is executed via `sh -c` and
   stdout is used as the key. Results are cached for the process lifetime.
   ```toml
   api_key = "!security find-generic-password -s deepseek-api-key -w"
   api_key = "!echo $DEEPSEEK_API_KEY"
   ```

2. **Environment variable name** — if the value does not start with `!`, rua
   first attempts to read it as an environment variable. If the variable is
   unset, the value is treated as a literal string. An explicit `$VAR` form
   is also accepted and errors instead of falling back when the variable is
   unset.
   ```toml
   api_key = "DEEPSEEK_API_KEY"
   api_key = "$DEEPSEEK_API_KEY"
   ```

3. **Literal string** — directly use the key text (not recommended for
   committed files).
   ```toml
   api_key = "sk-xxxxxxxx"
   ```

### `additional_params`

Arbitrary provider-specific request parameters, merged verbatim into every
completion request. Example (DeepSeek thinking toggle):

```toml
[[providers]]
name = "deepseek"
# ...
additional_params = { thinking = { type = "enabled" } }
```

## `server` section

| Key    | Default | Description                              |
|--------|---------|------------------------------------------|
| `port` | `3080`  | Daemon listen port (always `127.0.0.1`)  |

## Example config

```toml
# LM Studio local server
model = "local/qwen3.8-27b-uncensored-mlx"

[[providers]]
name = "local"
kind = "openai"
api_key = "lm-studio"
base_url = "http://127.0.0.1:1234/v1"

[server]
port = 3080
```

```toml
# DeepSeek API + GLM Coding Plan (OpenAI-compatible coding endpoint)
model = "deepseek/deepseek-v4-pro"

[[providers]]
name = "deepseek"
kind = "deepseek"
api_key = "!echo $DEEPSEEK_API_KEY"
base_url = "https://api.deepseek.com"

[[providers]]
name = "glm"
kind = "openai"
api_key = "$ZHIPUAI_API_KEY"
base_url = "https://open.bigmodel.cn/api/coding/paas/v4"
models = ["glm-5.3", "glm-5.3-flash"]
```
