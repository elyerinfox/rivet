# `invert`

Photo-negative the frame: negate luma and chroma. 8-bit `Yuv420p` only (the
default SDR output) — a 10-bit / HDR frame is rejected.

## Syntax

```text
invert
negate        # alias
```

```yaml
- invert
```

No parameters.

## Behaviour

Every sample becomes `255 − sample`, on **all three planes** (luma *and* chroma),
so colours invert to their complements as well as the tones — a true photographic
negative. Dimensions unchanged.

## Examples

```text
invert
invert,contrast=1.1
```

## Notes

- This negates chroma too, so reds become cyans, etc. To only flip tones you'd
  keep chroma and invert luma — not currently a separate filter.

Source: [`crates/codec/src/filter/invert.rs`](../../crates/codec/src/filter/invert.rs).
