# Gemini Direct Audio Fast Path

ZeroClaw can now answer some channel voice notes by sending the raw audio directly to Gemini for the live reply while a shadow transcription runs in parallel.

This is intentionally narrow in v1:

- only channel voice-note flows that already use the shared transcription runtime
- only when the active chat provider is Gemini
- only for inline audio input
- only for Telegram voice-note style messages and WhatsApp Web voice-note style messages
- no generic `[AUDIO:]` or `[VOICE:]` prompt markers yet

## What It Does

For a qualifying incoming voice note:

1. ZeroClaw starts Gemini live-audio inference immediately.
2. ZeroClaw starts the existing transcription runtime immediately.
3. If Gemini succeeds first, the assistant reply is sent to the channel immediately.
4. The transcript is still awaited in the background and becomes the canonical stored user turn.
5. If transcription fails after Gemini already replied, ZeroClaw stores `[Voice note received]` as the fallback canonical user text.

Raw audio is transient only. It is not written to session history, memory stores, or durable session backends.

## Requirements

The fast path only activates when all of these are true:

- the active provider supports inline audio input
- the active provider route resolves to Gemini
- shared transcription is enabled and configured well enough to produce a transcript
- the message is a qualifying channel voice note
- the audio duration is within `transcription.max_duration_secs`
- the MIME type is allowed by both:
  - the shared transcription runtime
  - the Gemini inline-audio allowlist used by ZeroClaw
- the raw audio bytes are below the Gemini fast-path inline threshold

If any guard fails, ZeroClaw falls back to the existing STT-only behavior.

## Basic Configuration

You need both:

- Gemini as the active chat provider for the sender/session/route
- transcription enabled with a working transcription provider

Example transcription setup:

```toml
[transcription]
enabled = true
default_provider = "gemini"
max_duration_secs = 600

[transcription.gemini]
api_key = "YOUR_GEMINI_API_KEY"
model = "gemini-3-flash-preview"
```

If you need help configuring Gemini transcription auth, see [gemini-transcription-setup.md](gemini-transcription-setup.md).

You also need your channel runtime to be using Gemini for chat. ZeroClaw will not use the direct-audio fast path when the active provider is OpenAI, Anthropic, OpenRouter, Ollama, or any other non-Gemini route.

## How To Use It

1. Configure shared transcription and make sure it works.
2. Make sure the channel route for the conversation is Gemini.
3. Send a Telegram or WhatsApp Web voice note in a flow that already used the shared transcription runtime before this change.
4. ZeroClaw will choose between:
   - direct audio + shadow transcript
   - normal STT-only behavior

There is no separate command to enable the fast path per message. It is selected automatically when the message qualifies.

## What Gets Stored

On a successful fast-path turn:

- the durable user turn is the transcript text
- the durable assistant turn is the assistant reply
- the assistant turn is never persisted before the canonical user turn is decided

If Gemini succeeds but transcription fails irrecoverably:

- the assistant reply is kept
- the durable user turn becomes `[Voice note received]`

If Gemini fails but transcription succeeds:

- ZeroClaw retries exactly once on the normal text path using the transcript
- if that retry succeeds, the transcript is stored first and the assistant reply second

If both Gemini live-audio and transcription fail:

- ZeroClaw surfaces the normal channel error
- no raw audio is persisted
- no assistant reply is persisted

## Current Scope Limits

This feature does not currently apply to:

- arbitrary uploaded audio files
- generic channel attachments
- non-channel chat surfaces
- non-Gemini live providers
- prompt markers that inject transcript text into the live audio request

The live Gemini request uses:

1. a fixed cue text
2. the inline audio payload

It does not include transcript text in the live request.

## Operational Behavior

ZeroClaw logs structured fast-path outcomes, including:

- attempted
- skipped with reason
- live Gemini audio succeeded or failed
- shadow transcript succeeded or failed
- fallback retry attempted, succeeded, or failed
- MIME type
- byte size
- duration when available
- placeholder persistence when transcript recovery fails after a successful live reply

Useful skip reasons include:

- provider does not support inline audio input
- transcription disabled or unavailable
- MIME not supported
- duration exceeds transcription limit
- audio bytes exceed the Gemini inline threshold

## Privacy and Persistence Notes

- `transient_audio` is kept out of serde persistence paths
- debug output for transient audio prints metadata only, never the bytes
- the canonical persisted conversation remains text-only after the turn completes
- future conversation context sees transcript text or `[Voice note received]`, not raw audio

## Troubleshooting

If you expected the fast path but got normal STT-only behavior:

- confirm the active provider is Gemini
- confirm transcription is enabled
- confirm the voice note came from Telegram or WhatsApp Web voice-note handling
- confirm the MIME type is supported
- confirm the duration is below `transcription.max_duration_secs`
- confirm the audio size is below the Gemini inline threshold

If you need the exact runtime knobs, see:

- [config-reference.md](../reference/api/config-reference.md)
- [channels-reference.md](../reference/api/channels-reference.md)
- [providers-reference.md](../reference/api/providers-reference.md)
