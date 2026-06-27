# Video filters

Per-frame geometric / colour transforms applied to the decoded source **once**,
before fan-out + per-rung scaling — so a filter applies to every rendition. They
transform the *source*, and the per-rung scaler then resizes the result to each
rung. (So if a crop changes the aspect ratio, set the rung dimensions to match.)

## The filters

**Geometry** — pure sample rearrangement, work on any bit depth (8- or 10-bit):

| Filter | Parameters | Effect |
|--------|------------|--------|
| `crop` | `w`, `h`, optional `x`, `y` | Crop a `w×h` region; without `x`/`y` it is centred. |
| `pad` | `w`, `h`, optional `x`, `y` | Letterbox / pillarbox into a `w×h` canvas (centred, neutral black). |
| `hflip` | — | Mirror horizontally. |
| `vflip` | — | Mirror vertically. |
| `rotate` | `90` \| `180` \| `270` | Rotate clockwise; 90 / 270 swap width↔height. |
| `grayscale` | — | Drop chroma. |

**Overlay & colour** — work on 8-bit `Yuv420p` (the default SDR output):

| Filter | Parameters | Effect |
|--------|------------|--------|
| `overlay` | `image` (PNG path), optional `x`, `y` | Alpha-composite a PNG (logo / watermark) at top-left `(x, y)`. The image is loaded + converted once; its alpha channel controls the blend. |
| `invert` | — | Negate (invert) luma + chroma. Alias: `negate`. |
| `brightness` | offset `-255..=255` | Add a luma offset (brighten / darken). |
| `contrast` | factor (`1.0` = unchanged) | Scale luma contrast around mid-grey. |
| `saturation` | factor (`0` = grayscale, `1.0` = unchanged) | Scale chroma. |

4:2:0 alignment means crop / pad / overlay sizes round to even. A chain is
validated when the spec is built — a bad value like `rotate=45` is rejected up
front, not at encode time. The overlay PNG is opened + decoded at that point too,
so a missing or unreadable image fails the job immediately rather than mid-encode.

## Two interchangeable forms

A chain is a list of [`codec::filter::VideoFilter`](../crates/codec/src/filter.rs)
values with two serializations that round-trip exactly
(`parse_chain(&chain_to_string(c)) == c`) — use whichever fits the surface.

### String chain (ffmpeg `-vf` style)

Comma-separated, each `name` or `name=a:b:…`:

```text
crop=W:H[:X:Y]   pad=W:H[:X:Y]   hflip   vflip   rotate=90|180|270   grayscale
overlay=PATH[:X:Y]   invert   brightness=N   contrast=F   saturation=F
```

e.g. `crop=1280:720,hflip,rotate=90` or `overlay=logo.png:24:24,saturation=1.2`.
`gray`/`transpose`/`negate` are accepted aliases (`gray` = `grayscale`,
`transpose` = `rotate=90`, `negate` = `invert`). This is the form every
**string** surface uses (CLI flag, IPC header, HTTP query string). An overlay
`PATH` can't contain `:` in the string form — use the structured form for paths
that do.

### Structured objects (YAML / JSON)

The batch manifest and the HTTP JSON `spec` body also accept the same filters as a
**list of objects** — unit filters as bare strings, parameterised ones as a
tagged object:

```yaml
filter:
  - crop:
      w: 1280
      h: 720          # x/y optional → centred
  - hflip
  - rotate: 90
  - overlay:
      image: assets/logo.png
      x: 24
      y: 24
  - saturation: 1.2
```

```json
"filter": [{ "crop": { "w": 1280, "h": 720 } }, "hflip", { "rotate": 90 }]
```

Both forms resolve to the same validated `Vec<VideoFilter>`.

### Watermark example

A bottom-anchored logo plus a gentle saturation bump, as a string chain:

```text
overlay=assets/logo.png:24:24,saturation=1.1
```

The PNG's own alpha channel is the blend mask, so a transparent logo composites
cleanly. Overlay + the colour filters require 8-bit SDR output (the default); on a
10-bit/HDR job they error rather than silently misbehave.

## Per-surface usage

| Surface | How | Forms |
|---------|-----|-------|
| CLI `transcode` / `pipe` | `--filter "crop=1280:720,hflip"` | string |
| Batch manifest | `filter:` ([batch DSL](batch.md#per-job-keys)) | string **or** object list |
| HTTP query | `?filter=crop=1280:720,hflip` | string |
| HTTP JSON `spec` | `"filter"` ([HTTP API](api.md)) | string **or** object list |
| IPC header | `#rivet filter=crop=1280:720,hflip` | string |
| Library | `spec.with_filters(…)` | `Vec<VideoFilter>` |

### Library

```rust
use codec::filter::{VideoFilter, parse_chain};

// build the structs directly…
let spec = OutputSpec::single_file(rungs).with_filters(vec![
    VideoFilter::Crop { w: 1920, h: 1080, x: None, y: None },
    VideoFilter::HFlip,
]);
// …or parse the string form
let spec = OutputSpec::single_file(rungs)
    .with_filters(parse_chain("crop=1920:1080,hflip")?);
```

See the [`OutputSpec` guide](output-spec.md#6-video-filters--with_filters) for where
filters sit among the other job settings. Implementation:
[`codec::filter`](../crates/codec/src/filter.rs).
