# LumeSub

A macOS app that transcribes your Mac's system audio in real time, translates it, and shows the result as overlay subtitles. Follow videos or online meetings with live translated captions.

## Features

- System audio capture (ScreenCaptureKit)
- Real-time transcription via Meta Model API (25 languages, auto-detect available)
- Context-aware translation (Chat Completions)
- Transparent subtitle overlays for source and translation, plus a control window
- Translation window split into finalized lines (upper, white) and in-progress lines (lower, gray)
- Readable sentence lifetime (4–6 seconds based on reading speed), one sentence per line
- Previous sentences stay visible while new ones arrive; late results are rescued instead of dropped
- UI available in 25 languages

## Requirements

- macOS 12.3 or later
- A Meta Model API key (see the [official docs](https://dev.meta.ai))

## Install

Download the `.dmg` from GitHub Releases and move it into your Applications folder (signed and notarized).

To build from source:

```sh
cargo build --release
```

To create a distributable `.dmg`:

```sh
./scripts/build-dmg.sh
```

## Setup

1. On first launch, a settings dialog opens automatically. Enter your API key and save.
2. Restart the app (API settings take effect on next launch).
3. Press Start and grant the screen recording permission when prompted.

## Usage

In the control window, pick the input language (Auto = auto-detect) and the output language, then press **Start**.

- **Pause**: pauses capture (the session is kept, so you can resume)
- **Stop**: ends capture
- Subtitle windows can be dragged around and resized; their positions and sizes are saved on Quit/close and restored on next launch.

## Settings

Items you can change in the control window (saved immediately):

| Item | Description |
|---|---|
| Input / output language | 25 languages. Input supports Auto (auto-detect); output defaults to Japanese |
| Display language | UI language (25 languages, defaults to English) |
| Follow main screen | Subtitles follow the main display when ON |
| Fast translation | Translates partial results without waiting for finalization (more API calls) |
| Subtitle length | 40–400 chars each for source and translation (default 160) |
| Font / size | Per-language font choices for source and translation |
| Dev logs | Verbose logging to the terminal |

The settings dialog (API key, base URL, model names, subtitle visibility toggles), which opens automatically on first launch, requires a restart to take effect.

## Environment variables (optional)

You can use environment variables instead of the settings dialog. Priority: saved value → environment variable → default.

| Variable | Purpose | Default |
|---|---|---|
| `META_API_KEY` (or `MODEL_API_KEY`) | API key | none |
| `META_API_BASE_URL` | API base URL | `https://api.meta.ai/v1` |
| `ASR_MODEL` | Transcription model | `muse-voice-transcribe-1.0` |
| `TRANSLATE_MODEL` | Translation model | `muse-spark-1.3` |
| `UI_LANG` | Display language on first launch | `English` |

## Config file

`~/.config/lume-sub/store.json` (readable/writable by the owner only, as it contains the API key)

## Notes

- API usage is billed (transcription is $0.18/hour)
- Fast translation mode increases translation API calls

## Development

```sh
cargo test          # unit tests
cargo clippy --all-targets
cargo fmt --check
```

With Dev logs enabled, the app prints machine-readable `⏱` timing events to the terminal.
Save them to a file and run the coverage check to verify every utterance was transcribed
and every transcript was translated:

```sh
./target/debug/lume-sub 2>&1 | tee timing.log
cargo run -- --coverage timing.log
```
