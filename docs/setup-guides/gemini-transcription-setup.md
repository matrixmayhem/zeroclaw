# Gemini Transcription Setup

ZeroClaw now supports Gemini as a speech-to-text provider in the shared transcription subsystem.
This is separate from the existing Google STT provider.

## What Changed

The transcription stack now includes:

- A new `gemini` transcription provider.
- A new `[transcription.gemini]` config section.
- Runtime auth support for Gemini STT through:
  - a config API key
  - `GEMINI_API_KEY`
  - `GOOGLE_API_KEY`
  - a managed Gemini auth profile
  - Gemini CLI OAuth credentials
- Gemini transcription support in channel flows that already use the shared transcription runtime, including Telegram and WhatsApp Web voice-note handling.

## Quick Start

Edit your config file and set Gemini as the default transcription provider:

```toml
[transcription]
enabled = true
default_provider = "gemini"

[transcription.gemini]
api_key = "YOUR_GEMINI_API_KEY"
model = "gemini-3-flash-preview"
```

Then run:

```powershell
cargo test transcription --lib -j 1
```

## Minimal Config

The smallest explicit Gemini STT setup is:

```toml
[transcription]
enabled = true
default_provider = "gemini"

[transcription.gemini]
api_key = "YOUR_GEMINI_API_KEY"
model = "gemini-3-flash-preview"
```

Notes:

- `enabled = true` must be set or channel transcription stays disabled.
- `default_provider = "gemini"` selects Gemini for the shared transcription flow.
- `model` defaults to `gemini-3-flash-preview` if omitted by code defaults, but keeping it explicit is clearer.

## Auth Options

Gemini transcription resolves auth in this order:

1. `[transcription.gemini].api_key`
2. `GEMINI_API_KEY`
3. `GOOGLE_API_KEY`
4. managed Gemini auth profile
5. Gemini CLI OAuth credentials

If multiple sources are present, the first match wins.

For most setups, the config API key or `GEMINI_API_KEY` is the simplest option.

### Option 1: Config API Key

```toml
[transcription]
enabled = true
default_provider = "gemini"

[transcription.gemini]
api_key = "YOUR_GEMINI_API_KEY"
model = "gemini-3-flash-preview"
```

### Option 2: Environment Variable

Keep the config section without a key:

```toml
[transcription]
enabled = true
default_provider = "gemini"

[transcription.gemini]
model = "gemini-3-flash-preview"
```

Then set one of:

```powershell
$env:GEMINI_API_KEY = "YOUR_GEMINI_API_KEY"
```

or

```powershell
$env:GOOGLE_API_KEY = "YOUR_GOOGLE_API_KEY"
```

Use `GEMINI_API_KEY` if you want the intent to stay explicit.

## Request Behavior

Gemini transcription uses two request modes:

- Inline audio for normal requests.
- Gemini Files API upload when the serialized inline request would exceed Gemini's inline request limit and API-key auth is being used.

Current limits and behavior:

- ZeroClaw still enforces the shared 25 MB audio cap.
- Gemini inline request handling switches at about 20 MB of serialized request size.
- OAuth-based Gemini transcription currently supports inline audio only.
- If OAuth auth is used and the request is too large for inline mode, ZeroClaw returns a clear error instead of silently retrying another path.

## Proxy Routing

Gemini STT requests use the runtime proxy scope:

```toml
transcription.gemini
```

If you use runtime proxy rules, make sure Gemini transcription traffic is allowed for that scope.

## Channel Usage

Once transcription is enabled and Gemini is configured as the default provider, channels that already call into the shared transcription runtime will use Gemini automatically for voice/audio transcription.

That includes the updated Telegram and WhatsApp Web transcription paths.

No channel-specific Gemini toggle is required beyond enabling transcription and setting the default transcription provider.

## Verify Setup

Run the targeted transcription tests:

```powershell
cargo test transcription --lib -j 1
```

A successful run confirms:

- Gemini provider registration
- Gemini auth precedence
- Gemini response parsing
- size-routing behavior
- config roundtrip and encrypted API key handling

## Troubleshooting

### `Default transcription provider 'gemini' is not configured`

Add the Gemini section:

```toml
[transcription.gemini]
model = "gemini-3-flash-preview"
```

and provide auth through either `api_key`, `GEMINI_API_KEY`, `GOOGLE_API_KEY`, managed auth, or CLI OAuth.

### `No Gemini STT authentication found`

Set one of:

- `[transcription.gemini].api_key`
- `GEMINI_API_KEY`
- `GOOGLE_API_KEY`

If you expect OAuth-based auth to work, confirm your managed Gemini profile or Gemini CLI credentials are actually present.

### Large audio fails under OAuth

That is expected with the current implementation.
Use API-key auth for large requests that need Gemini Files API fallback.

### Gemini vs Google STT confusion

Use:

- `default_provider = "gemini"` for Gemini STT
- `default_provider = "google"` for the Google STT provider

They are configured independently:

```toml
[transcription.gemini]
...

[transcription.google]
...
```
