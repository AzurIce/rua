# Configuration

rua reads its configuration from `~/.config/rua/config.toml`. The file is
auto-created with sensible defaults on first run if it does not exist.

## Config path

```
~/.config/rua/config.toml
```

You can also inspect the resolved path at runtime via
`rua::config::config_path()`.

## `deepseek` section

| Key       | Default                     | Description                           |
|-----------|----------------------------|---------------------------------------|
| `api_key` | `"!echo $DEEPSEEK_API_KEY"` | DeepSeek API key (see formats below)  |
| `base_url`| `"https://api.deepseek.com"`| API base URL                          |
| `model`   | `"deepseek-v4-pro"`         | Model name passed to the API          |

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
   unset, the value is treated as a literal string.
   ```toml
   api_key = "DEEPSEEK_API_KEY"
   ```

3. **Literal string** — directly use the key text (not recommended for
   committed files).
   ```toml
   api_key = "sk-xxxxxxxx"
   ```

## Example config

```toml
[deepseek]
api_key = "!echo $DEEPSEEK_API_KEY"
base_url = "https://api.deepseek.com"
model = "deepseek-v4-pro"
```
