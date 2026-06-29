# `hflip`

Mirror the frame horizontally (left ↔ right). Pure sample rearrangement — each
row is reversed — so it works at any bit depth (8- or 10-bit) and costs nothing
but a copy.

## Syntax

```text
hflip
```

```yaml
- hflip
```

No parameters.

## Behaviour

Reverses the sample order of every row in all three planes (Y, U, V). Dimensions
are unchanged. Apply twice to get back the original.

## Examples

```text
hflip                  # mirror
hflip,crop=1280:720    # mirror, then centre-crop
```

## Notes

- The vertical counterpart is [vflip](vflip.md); a 180° rotation
  ([rotate](rotate.md)) is `hflip` + `vflip` composed.

Source: [`crates/codec/src/filter/hflip.rs`](../../crates/codec/src/filter/hflip.rs).
