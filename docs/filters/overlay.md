# `overlay`

Alpha-composite a PNG (logo / watermark) onto the frame at a top-left position.
Unlike the other filters, `overlay` is a **resource filter**: the image is loaded
and converted **once** (by `FilterChain::prepare`), then composited per frame.
8-bit `Yuv420p` only (the default SDR output).

## Syntax

```text
overlay=PATH            # at (0, 0)
overlay=PATH:X:Y        # at (X, Y)
```

```yaml
- overlay: { image: assets/logo.png, x: 24, y: 24 }
```

> In the **string** form, `PATH` can't contain `:` (it's the arg separator) — use
> the structured (YAML/JSON) form for paths with colons (e.g. Windows drives).

## Parameters

| Param | Type | Meaning |
|-------|------|---------|
| `image` | path | A PNG, with or without an alpha channel. |
| `x` | `u32` | Left position of the overlay in the frame (default 0, rounds to even). |
| `y` | `u32` | Top position (default 0, rounds to even). |

## Behaviour

- **Prepared once.** When the chain is built, the PNG is opened, decoded to RGBA,
  and converted to BT.709 limited-range YUV 4:2:0 plus a per-sample alpha (luma
  resolution, and a 2×2-averaged alpha for chroma). A missing or undecodable
  image fails the **job immediately**, not mid-encode.
- **Per-frame composite.** `out = src·(1 − α) + overlay·α` on luma and chroma.
  Fully transparent samples (`α = 0`) are skipped — so a transparent PNG
  composites cleanly with no halo. The overlay is clipped to the frame bounds.
- **8-bit only**: on a 10-bit/HDR job it errors rather than silently misbehaving.

## Examples

```text
overlay=assets/logo.png:24:24
overlay=assets/logo.png:24:24,saturation=1.1   # logo, then a gentle colour bump
```

```yaml
filter:
  - overlay: { image: assets/logo.png, x: 24, y: 24 }
  - saturation: 1.1
```

## Notes

- The PNG's own alpha channel is the blend mask — design the watermark with the
  transparency you want; there is no separate opacity knob.
- Because it's a resource filter, `apply()` alone errors for `overlay` — it must
  go through a prepared [`FilterChain`](../../crates/codec/src/filter/mod.rs). The
  pipeline always does this, so this only matters to direct library callers.

Source: [`crates/codec/src/filter/overlay.rs`](../../crates/codec/src/filter/overlay.rs).
