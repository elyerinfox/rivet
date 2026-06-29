# `saturation`

Scale colour intensity by stretching the chroma planes around neutral. 8-bit
`Yuv420p` only.

## Syntax

```text
saturation=F      # F = factor, 1.0 = unchanged, 0 = grayscale
```

```yaml
- saturation: 1.2
```

## Parameters

| Param | Type | Meaning |
|-------|------|---------|
| (value) | `f32` | Saturation factor. `0` = grayscale, `1.0` = unchanged, `>1` = more vivid. |

## Behaviour

Both chroma planes are scaled around neutral (128): `chroma = clamp((chroma −
128)·F + 128, 0, 255)`. **Luma is untouched**, so brightness/contrast are
preserved while colour intensity changes. `F = 0` collapses chroma to neutral
(grayscale); large `F` over-saturates and can clip.

## Examples

```text
saturation=1.2     # a little more vivid
saturation=0.6     # muted, film-like colour
saturation=0       # grayscale (8-bit)
```

## Notes

- `saturation=0` greyscales at 8-bit; the dedicated [grayscale](grayscale.md)
  filter does the same as a pure rewrite at **any** bit depth.
- 8-bit SDR only — a 10-bit/HDR frame is rejected.

Source: [`crates/codec/src/filter/saturation.rs`](../../crates/codec/src/filter/saturation.rs).
