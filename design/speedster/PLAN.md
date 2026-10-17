# Speedster mascot ("the speedster runs")

A cute chibi version of the app-icon speedster that lives in the Dock (and optionally the menu bar)
and shows what your agents are doing. Zero model tokens at runtime: it only reads local state.

## Character

Chibi proportions of the icon character (`dist/macos/icon-1024.png`): oversized helmet with the
single swept-back fin and a glowing visor slit, small body, electric-blue painterly low-poly facets,
white-blue lightning accents. Friendly, not menacing. Original design: no ear wings, no chest bolt,
nothing from existing comic speedsters.

## States

| State | When | Frames | Motion |
|---|---|---|---|
| idle | nothing running | 2 (breathe A/B) | slow breathing |
| sleeping | everything settled, or the Mac idle for a long time | 2 (zZ A/B) | slow |
| jog | 1–3 agents working | 4 (run cycle) | ~8 fps |
| sprint | 4+ agents working | 4 (leaning run cycle) | ~12 fps, procedural lightning trail |
| alert | something in the Needs-you inbox | 2 (waving, spark) | gentle bounce |
| celebrate | a thread just finished | 2 (fist pump, lightning burst) | shown ~2 s, then back |
| tired | a chat is Limited | 1 (hands on knees, panting) | still |
| oops | a run failed | 1 (tripped) | still, ~2 s |

Priority when several apply: alert > oops > celebrate > tired > sprint > jog > idle > sleeping.

## Assets

- Generated with Codex image generation (the owner's ChatGPT plan, no API spend): first a character
  reference sheet, then every frame with the reference attached for consistency. 1024 px PNG with a
  transparent background, character centred with feet on a common baseline.
- Post-processing (Pillow): drop the generator's faint alpha haze, trim, scale every frame by one
  factor (so the character never changes size between frames), centre, put the feet on a shared
  baseline, and export `<frame>@512.png` / `@256.png` (256-colour palette) for the Dock and
  `@44.png` / `@22.png` for the menu bar. 72 files, about 1.1 MiB, embedded in the binary. Raw
  generations stay in `design/speedster/raw/` (ignored); the curated set is in
  `crates/ui/assets/speedster/`.

## In the app

- Dock: swap `NSApp.applicationIconImage` frames on a timer only while an animated state is active;
  idle and sleeping are static. Cap at 12 fps.
- The finished-thread lightning crackle along the sidebar row is a GPUI animation, not an image.
- Settings → Appearance: "Animated Dock icon" (on by default) and "Show speedster in menu bar" (off by
  default). Animation stops in Low Power Mode and when Reduce Motion is on.
- State comes from the engine data the app already has (session statuses, attention count, limited
  chats), so it works the same for chats running on another device.
