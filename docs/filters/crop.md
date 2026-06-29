# `crop`

Cut a `w×h` rectangle out of the frame. With an explicit `x`/`y` it crops at that
top-left offset; without, it crops **centred** (and clamps `w`/`h` to the frame
so an over-large request can't fail). Pure sample rearrangement, so it works at
any bit depth (8- or 10-bit).

## Syntax

```text
crop=W:H          # centred
crop=W:H:X:Y      # at top-left (X, Y)
```

```yaml
- crop: { w: 1280, h: 720 }            # centred
- crop: { w: 1280, h: 720, x: 0, y: 0 }
```

## Parameters

| Param | Type | Meaning |
|-------|------|---------|
| `w` | `u32` | Output width. |
| `h` | `u32` | Output height. |
| `x` | `u32?` | Left offset of the crop window. Omit (with `y`) to centre. |
| `y` | `u32?` | Top offset. |

## Behaviour

- **Centred mode** (no `x`/`y`): `w`/`h` are clamped to the frame size, then the
  window is centred — `x = (frame_w − w) / 2`, `y = (frame_h − h) / 2`.
- **Explicit mode**: the window is `[x, x+w) × [y, y+h)`. It must fit inside the
  frame — an out-of-bounds crop is a hard error (rejected when the chain is
  validated, before encoding).
- **Even alignment**: `x`, `y`, `w`, `h` all round **down** to even so the 4:2:0
  chroma planes (half resolution) stay aligned. Chroma is cropped at half the
  offset/size of luma.

## Examples

```text
crop=1280:720                  # centre-crop to 720p
crop=1920:800                  # crop a 2.40:1 letterbox out of 1080p
crop=640:480:100:50            # 640×480 starting at (100, 50)
```

Cropping changes the *source* aspect ratio — set the rung dimensions to match
(the per-rung scaler resizes the cropped result to each rung).

## Notes

- Cropping does not re-encode anything itself; it rearranges plane bytes, so it's
  cheap and bit-depth-agnostic.
- To go the other way (add borders instead of removing them), see [pad](pad.md).

Source: [`crates/codec/src/filter/crop.rs`](../../crates/codec/src/filter/crop.rs).
