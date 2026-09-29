# Provider Authentication

Most hosted providers support one or both of these authentication methods:

- Sign in through a browser or device flow backed by OAuth.
- Provide an API key.

Use `/login [provider]` to see the methods supported by a provider. Amazon Bedrock and Google Vertex AI can also use ambient cloud credentials.

## Authenticate interactively

Run `/login` and select a provider. Pi guides you through its OAuth or API-key flow and saves the resulting credential in [`auth.json`](configuration.md#agent-directory).

On a remote or headless machine, an OAuth callback may not reach the local process. When prompted, paste the final redirect URL or authorization code back into Pi.

Run `/logout` and select a provider to remove its stored credential. This does not unset environment variables, remove authentication from `models.json`, or revoke the credential at the provider.

`auth.json` can contain API keys and OAuth tokens. Keep it private and do not commit it.

Radius authentication uses its gateway catalog and caches refreshed model metadata for later offline startup. A custom Radius gateway configured in `models.json` uses its own catalog rather than inheriting the public `radius.pi.dev` catalog.

## Use an API key from the environment

Environment variables are useful in CI and anywhere Pi should not store the key. Set the variable before starting Pi:

```bash
export ANTHROPIC_API_KEY=sk-ant-...
pi
```

This table covers providers with a single primary API-key variable. Providers that need additional configuration or support ambient credentials are covered under [Cloud providers](#cloud-providers).

| Provider | Environment variable |
|---|---|
| Anthropic | `ANTHROPIC_API_KEY` |
| Ant Ling | `ANT_LING_API_KEY` |
| OpenAI | `OPENAI_API_KEY` |
| DeepSeek | `DEEPSEEK_API_KEY` |
| NVIDIA NIM | `NVIDIA_API_KEY` |
| Google Gemini | `GEMINI_API_KEY` |
| GitHub Copilot | `COPILOT_GITHUB_TOKEN` |
| Mistral | `MISTRAL_API_KEY` |
| Groq | `GROQ_API_KEY` |
| Cerebras | `CEREBRAS_API_KEY` |
| xAI | `XAI_API_KEY` |
| OpenRouter | `OPENROUTER_API_KEY` |
| Vercel AI Gateway | `AI_GATEWAY_API_KEY` |
| ZAI Coding Plan (Global) | `ZAI_API_KEY` |
| ZAI Coding Plan (China) | `ZAI_CODING_CN_API_KEY` |
| OpenCode Zen and Go | `OPENCODE_API_KEY` |
| Radius | `RADIUS_API_KEY` |
| Hugging Face | `HF_TOKEN` |
| Fireworks | `FIREWORKS_API_KEY` |
| Together AI | `TOGETHER_API_KEY` |
| Baseten | `BASETEN_API_KEY` |
| Kimi For Coding | `KIMI_API_KEY` |
| Meta | `META_API_KEY` |
| MiniMax | `MINIMAX_API_KEY` |
| MiniMax (China) | `MINIMAX_CN_API_KEY` |
| Moonshot AI (Global and China) | `MOONSHOT_API_KEY` |
| Qwen Token Plan and Individual | `QWEN_TOKEN_PLAN_API_KEY` |
| Qwen Token Plan (China) | `QWEN_TOKEN_PLAN_CN_API_KEY` |
| Xiaomi MiMo | `XIAOMI_API_KEY` |
| Xiaomi MiMo Token Plan (China) | `XIAOMI_TOKEN_PLAN_CN_API_KEY` |
| Xiaomi MiMo Token Plan (Amsterdam) | `XIAOMI_TOKEN_PLAN_AMS_API_KEY` |
| Xiaomi MiMo Token Plan (Singapore) | `XIAOMI_TOKEN_PLAN_SGP_API_KEY` |

Anthropic also recognizes `ANTHROPIC_OAUTH_TOKEN` as an API credential and `ANTHROPIC_AUTH_TOKEN` as bearer authentication.

## Load an API key from a command

To use a secret manager without writing the resolved key to disk, set a provider's `key` in `auth.json` to a command prefixed with `!`:

```json
{
  "anthropic": {
    "type": "api_key",
    "key": "!security find-generic-password -ws 'anthropic'"
  }
}
```

Pi runs the command when the key is first needed and caches its standard output for the process lifetime. Empty output, a timeout, or a nonzero exit leaves the key unresolved until Pi restarts.

## Cloud Providers

The providers below need additional settings or can use credentials supplied by their cloud platform.

A stored API-key credential can include an `env` object. Its values take priority over the process environment for that provider:

```json
{
  "cloudflare-workers-ai": {
    "type": "api_key",
    "key": "...",
    "env": {
      "CLOUDFLARE_ACCOUNT_ID": "account-id"
    }
  }
}
```

### Azure OpenAI

Set an API key plus either a base URL or resource name:

```bash
export AZURE_OPENAI_API_KEY=...
export AZURE_OPENAI_BASE_URL=https://your-resource.ai.azure.com
# Or:
export AZURE_OPENAI_RESOURCE_NAME=your-resource
```

Resource root URLs under `ai.azure.com`, `cognitiveservices.azure.com`, and `openai.azure.com` are normalized to the OpenAI API path.

### Amazon Bedrock

Bedrock can use a bearer token or an ambient AWS credential source:

```bash
# Named profile
export AWS_PROFILE=your-profile

# IAM keys
export AWS_ACCESS_KEY_ID=AKIA...
export AWS_SECRET_ACCESS_KEY=...
# Required for temporary credentials
export AWS_SESSION_TOKEN=...

# Bedrock bearer token
export AWS_BEARER_TOKEN_BEDROCK=...

# Region, when not supplied by the profile or AWS SDK configuration
export AWS_REGION=us-west-2
# AWS_DEFAULT_REGION is also supported
```

Pi also supports ECS task credentials and IRSA through the standard `AWS_CONTAINER_CREDENTIALS_*` and `AWS_WEB_IDENTITY_TOKEN_FILE` variables.

### Cloudflare AI Gateway

The gateway requires a token, account ID, and gateway ID:

```bash
export CLOUDFLARE_API_KEY=...
export CLOUDFLARE_ACCOUNT_ID=...
export CLOUDFLARE_GATEWAY_ID=...
```

The account and gateway IDs can come from the process environment or the credential's `env` object in `auth.json`.

`CLOUDFLARE_API_KEY` authenticates Pi to the gateway. Upstream access can use Cloudflare unified billing, credentials stored in the gateway, or an `Authorization` header configured for the provider in `models.json`.

### Cloudflare Workers AI

Workers AI requires a token and account ID:

```bash
export CLOUDFLARE_API_KEY=...
export CLOUDFLARE_ACCOUNT_ID=...
```

The account ID can also be stored in the credential's `env` object.

### Google Vertex AI

Use a Google Cloud API key:

```bash
export GOOGLE_CLOUD_API_KEY=...
```

To use Application Default Credentials, configure a project and location:

```bash
export GOOGLE_CLOUD_PROJECT=your-project
# GCLOUD_PROJECT is also supported
export GOOGLE_CLOUD_LOCATION=us-central1
```

Then authenticate:

```bash
gcloud auth application-default login
```

To use a service-account key file instead, set `GOOGLE_APPLICATION_CREDENTIALS` along with the project and location.

## Claude Code CLI (`claude-code`)

The `claude-code` provider sends model calls through your installed, logged-in [Claude Code](https://docs.claude.com/en/docs/claude-code) CLI in headless mode (`claude -p --output-format stream-json`) instead of an API key. It needs no configuration: when a `claude` executable is on `PATH` (or named by `ULTRON_CLAUDE_CODE_BIN`), `claude-code/opus`, `claude-code/sonnet` and `claude-code/haiku` are available. These ids are the CLI's own model aliases and are only matched by their full `claude-code/...` reference, so `--model sonnet` keeps meaning the other providers' Sonnet models.

The CLI authenticates itself. Ultron never reads, copies or creates Claude credentials; it passes `CLAUDE_CONFIG_DIR` through to the child and, before the first call, checks `claude auth status --json`: a logged-out CLI fails with a message to run `claude auth login`, and a CLI set up for a third-party API provider (Bedrock, Vertex) is refused unless `ULTRON_CLAUDE_CODE_ALLOW_THIRD_PARTY=1`. If the CLI authenticates with an API key instead of a claude.ai login (`apiKeySource` in its stream), the call proceeds and the response carries a warning diagnostic.

Every call is isolated from your own Claude Code setup and uses only flags `claude --help` lists (checked once; a missing flag fails naming it): `--tools ""`, `--strict-mcp-config` with an empty `--mcp-config`, `--setting-sources ""` (no user or project settings, hooks or `CLAUDE.md`), `--disable-slash-commands`, `--no-session-persistence`, `--permission-prompts none`, an empty working directory, and `--system-prompt` with the caller's own system text (never Claude Code's default prompt). The CLI still adds a short identity line and environment/date reminders, about 350 input tokens per call, against about 6,600 with its default prompt. Thinking is off unless a thinking level is set, which becomes `--effort`.

**Tools.** `claude -p` returns completions, not raw tool calls. A tool-free request (`rlm.infer` and `rlm.map` frames, `/review` frames, judges) is one isolated completion. A lane that declares tools (the root agent under `ultron --claude`, `rlm.spawn` children that inherit its model, frames with `depth > 1`) runs as a Claude Code session in Ultron's session worker: the lane's tools are served to the CLI over MCP (`ultron mcp --bridge`), each call is run by Ultron's harness as an ordinary tool call, and the session is resumed with `--resume` from one run to the next (see the README's `ultron --claude` section). Outside a session worker such a lane fails before any process starts. Set `ULTRON_RLM_FRAME_MODEL=claude-code/haiku` to send code-free frames there by default, or pass `model="claude-code/haiku"` to one call.

A frame's JSON-schema contract becomes `--json-schema` (object schemas; the reply is the CLI's structured output). Other schemas, or one the API rejects, are asked for in the prompt instead. Images in user messages are passed as image blocks. Usage comes from the CLI's result event; the cost is the CLI's own `total_cost_usd` (under a subscription a list-price figure, marked `(sub)` in the footer), or unknown when it reports none.

**Fair use.** These calls draw on your subscription's shared five-hour and weekly limits. At most `ULTRON_CLAUDE_CODE_CONCURRENCY` calls (default 4) run at once. An exhausted window is reported as `Claude Code usage limit reached (...; resets at ...)`, is not retried, and makes an inference frame `Incomplete` with reason `usage_limit`; a transient 429 is retried like any provider's.

Each call is one `claude` process (about 0.6 to 0.9 s of CLI start-up, 1.3 to 1.6 s for a small Haiku call). A process is never reused: in stream-json mode it carries context from one prompt to the next. What is pooled is start-up: after a call, up to `ULTRON_CLAUDE_CODE_WARM` (default 2, `0` disables) idle processes with the same arguments are started ahead of time and stop after `ULTRON_CLAUDE_CODE_WARM_TTL_MS` (default 30000), which brings the next call of that shape to about 0.7 s. `ULTRON_CLAUDE_CODE_TIMEOUT_MS` (default 600000) bounds one call; an abort or timeout stops the process tree.

Model ids the aliases do not cover go in `models.json`:

```json
{
  "providers": {
    "claude-code": {
      "models": [
        {
          "id": "claude-opus-4-8",
          "api": "claude-code-cli",
          "reasoning": true,
          "input": ["text", "image"],
          "contextWindow": 200000,
          "maxTokens": 32000
        }
      ]
    }
  }
}
```
