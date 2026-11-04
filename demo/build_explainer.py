#!/usr/bin/env python3
"""Build the LumeSub multilingual explainer video.

Procedurally draws mascot 'Lume', lip-flaps it to per-language TTS audio,
and muxes everything into demo/explainer.mp4 (1280x720, 30fps).
"""
import json
import subprocess
import sys
import wave
from pathlib import Path

import numpy as np
from PIL import Image, ImageDraw, ImageFont

HERE = Path(__file__).parent
AUDIO = HERE / "audio"
W, H, FPS, SR = 1280, 720, 30, 48000
LEAD, GAP, TAIL = 4.0, 0.9, 4.0
FADE = 0.5

SEGS = json.load(open(HERE / "script.json"))


def load_font(size: int) -> ImageFont.FreeTypeFont:
    for p in ("/System/Library/Fonts/Helvetica.ttc",
              "/System/Library/Fonts/Supplemental/Arial.ttf"):
        try:
            return ImageFont.truetype(p, size)
        except OSError:
            continue
    return ImageFont.load_default()


FONT_BADGE = load_font(40)


# ---------- audio ----------
def to_wav(aiff: Path) -> np.ndarray:
    out = aiff.with_suffix(".wav")
    subprocess.run(["ffmpeg", "-y", "-v", "error", "-i", str(aiff),
                    "-ar", str(SR), "-ac", "1", "-c:a", "pcm_s16le",
                    str(out)], check=True)
    with wave.open(str(out), "rb") as f:
        raw = f.readframes(f.getnframes())
    return np.frombuffer(raw, dtype=np.int16).astype(np.float32) / 32768.0


def build_audio() -> tuple[np.ndarray, list[tuple[float, float, dict]]]:
    """Return (samples, [(start, end, seg), ...]) including gaps."""
    chunks: list[np.ndarray] = [np.zeros(int(SR * LEAD), np.float32)]
    bounds = []
    t = LEAD
    for i, seg in enumerate(SEGS):
        pcm = to_wav(AUDIO / f"{seg['id']}.aiff")
        rms = float(np.sqrt(np.mean(pcm ** 2)))
        print(f"{seg['id']}: {len(pcm)/SR:.2f}s rms={rms:.3f}")
        if rms < 0.005:
            raise SystemExit(f"segment {seg['id']} looks silent, aborting")
        chunks.append(pcm)
        bounds.append((t, t + len(pcm) / SR, seg))
        t += len(pcm) / SR
        gap = TAIL if i == len(SEGS) - 1 else GAP
        chunks.append(np.zeros(int(SR * gap), np.float32))
        t += gap
    return np.concatenate(chunks), bounds


# ---------- drawing ----------
def vgrad(size, top, bot) -> Image.Image:
    arr = np.zeros((size[1], size[0], 3), np.uint8)
    for c in range(3):
        arr[:, :, c] = np.linspace(top[c], bot[c], size[1])[:, None]
    return Image.fromarray(arr)


def make_bg() -> Image.Image:
    bg = vgrad((W, H), (14, 14, 42), (38, 26, 78))
    d = ImageDraw.Draw(bg, "RGBA")
    yy, xx = np.mgrid[0:H, 0:W]
    r = np.sqrt((xx - 640) ** 2 + (yy - 360) ** 2)
    glow = np.clip(1 - r / 520, 0, 1) ** 2
    glow_img = Image.new("RGBA", (W, H), (130, 140, 255, 0))
    glow_img.putalpha(Image.fromarray((glow * 70).astype(np.uint8)))
    bg = Image.alpha_composite(bg.convert("RGBA"), glow_img)
    d = ImageDraw.Draw(bg)
    rng = np.random.default_rng(7)
    for _ in range(26):
        x, y = rng.integers(0, W), rng.integers(0, H)
        rr = int(rng.integers(3, 14))
        d.ellipse([x - rr, y - rr, x + rr, y + rr],
                  fill=(255, 255, 255, int(rng.integers(8, 26))))
    return bg.convert("RGB")


def body_layer() -> tuple[Image.Image, Image.Image]:
    """Return (body_rgb, body_mask) without face, 1280x720 canvas coords."""
    layer = Image.new("RGBA", (W, H), (0, 0, 0, 0))
    mask = Image.new("L", (W, H), 0)
    dm = ImageDraw.Draw(mask)
    dm.rounded_rectangle([470, 150, 810, 560], radius=160, fill=255)
    grad = vgrad((W, H), (196, 205, 255), (127, 150, 255))
    layer = Image.composite(grad.convert("RGBA"), layer, mask)
    d = ImageDraw.Draw(layer)
    # belly
    belly_m = Image.new("L", (W, H), 0)
    ImageDraw.Draw(belly_m).ellipse([560, 340, 720, 525], fill=110)
    belly = Image.new("RGBA", (W, H), (235, 240, 255, 255))
    layer = Image.composite(belly, layer, belly_m)
    d = ImageDraw.Draw(layer)
    # feet
    d.rounded_rectangle([545, 540, 618, 592], radius=26, fill=(96, 116, 220, 255))
    d.rounded_rectangle([662, 540, 735, 592], radius=26, fill=(96, 116, 220, 255))
    # antenna
    d.line([640, 150, 640, 96], fill=(150, 165, 255, 255), width=10)
    return layer, mask


def draw_face(d: ImageDraw.ImageDraw, env: float, blink: float, t: float):
    # antenna tip glow pulse
    pulse = 0.6 + 0.4 * np.sin(2 * np.pi * t / 1.6)
    gr = int(26 + 10 * pulse)
    d.ellipse([640 - gr, 80 - gr, 640 + gr, 80 + gr],
              fill=(255, 220, 120, int(60 + 50 * pulse)))
    d.ellipse([622, 62, 658, 98], fill=(255, 214, 110, 255))
    # eyes (blink scales height)
    for cx in (590, 690):
        h = int(45 * blink)
        d.ellipse([cx - 45, 295 - h, cx + 45, 295 + h], fill=(255, 255, 255, 255))
        if blink > 0.3:
            pr = 20
            d.ellipse([cx - pr, 295 - pr, cx + pr, 295 + pr], fill=(40, 40, 70, 255))
            d.ellipse([cx - 3, 283 - 3, cx + 9, 283 + 9], fill=(255, 255, 255, 255))
    # cheeks
    d.ellipse([518, 348, 572, 380], fill=(255, 140, 170, 130))
    d.ellipse([708, 348, 762, 380], fill=(255, 140, 170, 130))
    # mouth
    open_h = (env ** 0.7) * 66
    if open_h < 7:
        d.arc([596, 372, 684, 428], start=20, end=160,
              fill=(60, 50, 90, 255), width=8)
    else:
        mw = 52 - open_h * 0.18
        d.ellipse([640 - mw, 392, 640 + mw, 392 + open_h * 2],
                  fill=(58, 43, 77, 255))
        if open_h > 26:
            d.ellipse([640 - mw * 0.55, 392 + open_h * 2 - 30,
                       640 + mw * 0.55, 392 + open_h * 2 - 6],
                      fill=(255, 130, 150, 255))


def badge(text: str) -> Image.Image:
    pad_x, pad_y = 34, 14
    bb = FONT_BADGE.getbbox(text)
    tw, th = bb[2] - bb[0], bb[3] - bb[1]
    img = Image.new("RGBA", (tw + pad_x * 2, th + pad_y * 2 + 8), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    d.rounded_rectangle([0, 0, img.width - 1, img.height - 1],
                        radius=30, fill=(255, 255, 255, 235))
    d.text((pad_x - bb[0], pad_y - bb[1]), text, font=FONT_BADGE,
           fill=(30, 30, 60, 255))
    return img


def main() -> None:
    pcm, bounds = build_audio()
    n_frames = int(len(pcm) / SR * FPS)

    # per-frame envelope with fast attack / slow release
    fr = len(pcm) // n_frames
    env = np.zeros(n_frames, np.float32)
    for i in range(n_frames):
        w = pcm[i * fr:(i + 1) * fr]
        env[i] = np.sqrt(np.mean(w ** 2)) if len(w) else 0.0
    peak = np.percentile(env, 97)
    env = np.clip(env / max(peak, 1e-4), 0, 1)
    sm = 0.0
    for i in range(n_frames):
        sm = env[i] if env[i] > sm else sm * 0.80
        env[i] = sm

    # which segment (for badge) each frame belongs to; gaps show next seg
    seg_of = np.zeros(n_frames, np.int32)
    for i in range(n_frames):
        t = i / FPS
        k = len(bounds) - 1
        for j, (s, e, _) in enumerate(bounds):
            if t < e + (TAIL if j == len(bounds) - 1 else GAP):
                k = j
                break
        seg_of[i] = k
    badges = [badge(f"{j+1}/{len(bounds)} · {s['badge']}")
              for j, (_, _, s) in enumerate(bounds)]

    bg = make_bg()
    body, _ = body_layer()

    wav_path = Path("/tmp/lume_explainer.wav")
    with wave.open(str(wav_path), "wb") as f:
        f.setnchannels(1)
        f.setsampwidth(2)
        f.setframerate(SR)
        f.writeframes((np.clip(pcm, -1, 1) * 32767).astype(np.int16).tobytes())

    cmd = ["ffmpeg", "-y", "-v", "error",
           "-f", "rawvideo", "-pix_fmt", "rgb24", "-s", f"{W}x{H}",
           "-r", str(FPS), "-i", "-",
           "-i", str(wav_path),
           "-map", "0:v", "-map", "1:a",
           "-c:v", "libx264", "-pix_fmt", "yuv420p", "-crf", "18",
           "-c:a", "aac", "-b:a", "160k", "-shortest",
           str(HERE / "explainer.mp4")]
    proc = subprocess.Popen(cmd, stdin=subprocess.PIPE)
    assert proc.stdin is not None

    for i in range(n_frames):
        t = i / FPS
        frame = bg.copy()
        bob = int(-11 * env[i] + 3 * np.sin(2 * np.pi * t / 2.6))
        char = body.copy()
        dc2 = ImageDraw.Draw(char)
        blink = 0.08 if (t % 3.4) < 0.12 else 1.0
        draw_face(dc2, float(env[i]), blink, t)
        frame.paste(char, (0, bob), char)
        b = badges[seg_of[i]]
        frame.paste(b, (40, 26), b)
        f = min(1.0, t / FADE, (n_frames / FPS - t) / FADE)
        if f < 1.0:
            frame = Image.fromarray(
                (np.asarray(frame, np.float32) * f).astype(np.uint8))
        proc.stdin.write(frame.convert("RGB").tobytes())
        if i % 300 == 0:
            print(f"frame {i}/{n_frames}", flush=True)
    proc.stdin.close()
    if proc.wait() != 0:
        raise SystemExit("ffmpeg failed")
    print("wrote", HERE / "explainer.mp4",
          f"{len(pcm)/SR:.1f}s", f"{n_frames} frames")


if __name__ == "__main__":
    main()
