# Configuration

rua reads its configuration from `~/.config/rua/config.toml`. The file is
auto-created with sensible defaults on first run if it does not exist.

## Config path

```
~/.config/rua/config.toml
```

You can also inspect the resolved path at runtime via
`rua_core::config::config_path()`.

## `provider` section

| Key                 | Default                        | Description                                  |
|---------------------|--------------------------------|----------------------------------------------|
| `kind`              | `"openai"`                     | `"openai"` = OpenAI-compatible endpoint (incl. LM Studio); `"deepseek"` = DeepSeek API |
| `api_key`           | `"lm-studio"`                  | API key (see formats below)                  |
| `base_url`          | `"http://127.0.0.1:1234/v1"`   | API base URL                                 |
| `model`             | `"qwen3.8-27b-uncensored-mlx"` | Model name passed to the API                 |
| `additional_params` | `{}`                           | Provider-specific request params, passed through verbatim |

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
[provider.additional_params]
thinking = { type = "enabled" }
```

## `server` section

| Key    | Default | Description                              |
|--------|---------|------------------------------------------|
| `port` | `3080`  | Daemon listen port (always `127.0.0.1`)  |

## Example config

```toml
# LM Studio local server (default)
[provider]
kind = "openai"
api_key = "lm-studio"
base_url = "http://127.0.0.1:1234/v1"
model = "qwen3.8-27b-uncensored-mlx"

[server]
port = 3080
```

```toml
# DeepSeek API
[provider]
kind = "deepseek"
api_key = "!echo $DEEPSEEK_API_KEY"
base_url = "https://api.deepseek.com"
model = "deepseek-v4-pro"
```
