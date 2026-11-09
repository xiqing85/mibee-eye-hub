# Inter-form IPC contract (design, not yet implemented)

**Status: forward contract for the M2 form split** (`mibee-talk` /
`mibee-mic` / `mibee-cam`). Nothing here is served today; the four
forms still run the full stack. This document is written BEFORE the
split so the seam gets designed once, with a contract — modeled on
hex's `docs/streams.md` ("clients written against this document
should not need to change when the engine becomes a separate
service"). When an implementation lands, update this file to describe
shipped behavior and drop this paragraph.

## Purpose

One host may run several mibee forms: `mibee-talk` (GB-only talkback
unit) and `mibee-mic` (mic/capture unit) on the same machine share
one microphone and one voice stack. The contract lets a form borrow
another form's engine over an authenticated loopback channel instead
of each loading its own copy of the models.

## Discovery and auth

- The serving form writes `ipc.json` into its data directory
  (`~/.local/share/mibee-eye/` unless redirected), mode 0600:
  `{"port": <u16>, "token": "<random>", "apiVersion": 1}`.
- Every request carries `Authorization: Bearer <token>`. A wrong or
  missing token answers `401` with no body. Ids are not guessable and
  are never enumerated: an unknown stream id and a wrong per-stream
  token both answer `404 stream-not-found`.
- The token file is regenerated at each start; clients re-read it
  per connection attempt, never cache it across restarts.

## Voice session resource (`POST /voice/sessions`)

Body: `{"sampleRate": 16000, "purpose": "talkback"|"interaction"}`.
`201` → `{"id": <u64>, "ownerToken": "…", "leaseMs": 10000}`. One
live session per serving form (`409 session-busy`); an ended session
does not count.

- `POST /voice/sessions/{id}/audio` — raw `f32le`/`s16le` mono 16 kHz
  chunks (≤256 KiB), `204` when queued, renews the lease.
- `GET  /voice/sessions/{id}/events` — SSE: `ready`, `wake` (keyword
  detected), `transcript` (text + the §54 audit object),
  `final`, `error`, `cancelled`. A newer subscriber replaces an
  older one; events replay from session start.
- `POST /voice/sessions/{id}/finish` / `heartbeat` / `cancel` —
  hex semantics: finish waits bounded then `504` while the session
  continues; heartbeat renews the lease; cancel discards.

## Scheduling: the local user always comes first

The serving form's own voice rounds (its KWS/ASR, SPEC appendix A
#55's latency-critical windows) preempt any borrowed session: the
borrower's audio waits in a bounded backlog while a #55 window is
open, catches up after, and a backlog over 120 s of audio answers
`429 session-backlog-full`. A borrowed session never delays the
serving form's wake word.

## Error codes

JSON `{"code","message"}` bodies; `401` has none. `400`
invalid-request / unsupported-sample-rate / invalid-audio; `401`;
`404 stream-not-found`; `409 session-busy / session-ended /
lease-expired / engine-unavailable`; `411 length-required`; `413`
audio-too-large; `429 session-backlog-full`; `503 shutting-down`.

## Limits

| Limit | Value |
| --- | --- |
| Live sessions | 1 |
| Audio request | 256 KiB |
| Backlog | 120 s |
| Lease | 10 s |
| Finish wait | 120 s |

## Non-goals

- No cross-host use: loopback only, the token never leaves the host.
- No model loading/switching through this API — the serving form's
  model selection (§4.9) is the only path; a session uses whatever
  the engine holds (`409 model-mismatch` if the client pinned one).
- Video streams are NOT part of this contract (RTSP already serves
  them); this is the voice-engine seam only.
