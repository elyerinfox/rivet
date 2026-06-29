# `vflip`

Mirror the frame vertically (top ↔ bottom). Pure sample rearrangement — the row
order is reversed — so it works at any bit depth (8- or 10-bit) and is just a
copy.

## Syntax

```text
vflip
```

```yaml
- vflip
```

No parameters.

## Behaviour

Reverses the row order of all three planes (Y, U, V). Dimensions are unchanged.
Apply twice to get back the original.

## Examples

```text
vflip                  # flip upside-down
vflip,hflip            # equivalent to rotate=180
```

## Notes

- The horizontal counterpart is [hflip](hflip.md); a 180° rotation
  ([rotate](rotate.md)) composes `hflip` + `vflip`.

Source: [`crates/codec/src/filter/vflip.rs`](../../crates/codec/src/filter/vflip.rs).
