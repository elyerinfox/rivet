# `grayscale`

Drop colour by setting both chroma planes to their neutral value, keeping luma.
Pure sample rewrite, so any bit depth (8- or 10-bit).

## Syntax

```text
grayscale
gray          # alias
```

```yaml
- grayscale
```

No parameters.

## Behaviour

Luma (Y) is left untouched; U and V are overwritten with neutral mid-range — 128
for 8-bit, 512 for 10-bit LE — which renders the image as true greyscale. The
frame stays in `Yuv420p` (it is *not* converted to a single-plane format), so the
rest of the pipeline is unchanged.

## Examples

```text
grayscale
gray,contrast=1.2      # desaturate, then boost contrast
```

## Notes

- This is the `saturation=0` special case done as a pure rewrite at any bit depth;
  [saturation](saturation.md) (8-bit) lets you partially desaturate instead.

Source: [`crates/codec/src/filter/grayscale.rs`](../../crates/codec/src/filter/grayscale.rs).
