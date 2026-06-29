# `contrast`

Scale luma contrast around mid-grey. 8-bit `Yuv420p` only.

## Syntax

```text
contrast=F        # F = factor, 1.0 = unchanged
```

```yaml
- contrast: 1.2
```

## Parameters

| Param | Type | Meaning |
|-------|------|---------|
| (value) | `f32` | Contrast factor. `1.0` = unchanged, `>1` = more contrast, `<1` = flatter, `0` = flat mid-grey. |

## Behaviour

`luma = clamp((luma − 128)·F + 128, 0, 255)` — values are pushed away from (or
pulled toward) mid-grey (128). **Chroma is untouched**, so only tonal contrast
changes. Highlights/shadows clip at the extremes as `F` grows.

## Examples

```text
contrast=1.2       # a gentle punch-up
contrast=0.8       # flatter, lower-contrast look
```

## Notes

- Pairs naturally with [brightness](brightness.md) (offset) and
  [saturation](saturation.md) (colour). 8-bit SDR only.

Source: [`crates/codec/src/filter/contrast.rs`](../../crates/codec/src/filter/contrast.rs).
