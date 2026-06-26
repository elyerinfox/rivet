//! Intel QSV (oneVPL) **10-bit** AV1 encoder — a hand-written P010 path.
//!
//! `shiguredo_vpl`'s high-level `Encoder`/`FrameFormat` only exposes 8-bit
//! formats (Nv12/Yuy2/Bgra), so this module drives oneVPL **directly** through
//! the crate's exposed raw bindings (`shiguredo_vpl::ffi`) to encode 10-bit
//! AV1. Intel Arc / Meteor Lake+ hardware supports P010 10-bit AV1 via oneVPL;
//! the only gap was the wrapper's format enum, which we sidestep here without
//! forking it.
//!
//! Output is web-safe AV1 **Main** profile, 4:2:0 10-bit; HDR is tagged at the
//! container level by the muxer (`colr`/`mdcv`/`clli`), so this just has to
//! produce the 10-bit bitstream. It mirrors the upstream 8-bit flow exactly
//! (loader → CQP video param → Query/Init → `MFXMemory_GetSurfaceForEncode` +
//! `FrameInterface` Map/Unmap → `EncodeFrameAsync` → `SyncOperation`), plus the
//! 10-bit-specific bits: `FourCC = P010`, `BitDepthLuma/Chroma = 10`,
//! **`Shift = 1`** (mandatory — without it the iHD driver reads P010 from the
//! low 10 bits and silently encodes 1/64-amplitude noise), and an
//! `mfxExtCodingOption3` with `TargetBitDepthLuma/Chroma = 10`.
//!
//! Verified-by-review: `shiguredo_vpl` doesn't build on a Windows MSVC host and
//! there's no Arc GPU here, so this is validated against the upstream's `sys`
//! usage, pending real Arc-hardware confirmation.

use std::collections::VecDeque;

use anyhow::{Result, bail};
use bytes::Bytes;

use shiguredo_vpl::AdapterSelector;
use shiguredo_vpl::ffi as sys;

use super::tuning;
use super::{EncodedPacket, Encoder, EncoderConfig};
use crate::frame::{PixelFormat, VideoFrame};

fn align_up(v: u32, a: u32) -> u32 {
    (v + a - 1) & !(a - 1)
}

/// oneVPL statuses < 0 are errors; 0 is `MFX_ERR_NONE`; > 0 are warnings.
fn check(status: i32, ctx: &str) -> Result<()> {
    if status < 0 {
        bail!("oneVPL {ctx} failed: status {status}");
    }
    Ok(())
}

unsafe fn frame_interface(surface: *mut sys::mfxFrameSurface1) -> *mut sys::mfxFrameSurfaceInterface {
    unsafe { (*surface).__bindgen_anon_1.FrameInterface }
}

pub struct QsvP010Encoder {
    config: EncoderConfig,
    loader: sys::mfxLoader,
    session: sys::mfxSession,
    width: u32,
    height: u32,
    bitstream_buf: Vec<u8>,
    collected: VecDeque<EncodedPacket>,
    frame_counter: u64,
    flushed: bool,
}

// Raw oneVPL session/loader pointers; the encoder is driven from one thread and
// the collector is owned. Matches the `Encoder: Send` bound the dispatcher needs.
unsafe impl Send for QsvP010Encoder {}

impl QsvP010Encoder {
    pub fn new(config: EncoderConfig, gpu_index: u32) -> Result<Self> {
        if config.pixel_format != PixelFormat::Yuv420p10le {
            bail!("QsvP010Encoder is the 10-bit path; got {:?}", config.pixel_format);
        }
        let AdapterSelector::DrmRenderNode(render_node) =
            crate::gpu::adapter_selector_for_gpu_index(gpu_index)?;

        let aligned_w = align_up(config.width, 16);
        let aligned_h = align_up(config.height, 16);
        let tp = tuning::qsv_av1_params(config.target, config.tier, config.width, config.height);

        unsafe {
            // --- loader: MFXLoad → MFXCreateConfig×2 (HW impl + DRM node) → MFXCreateSession ---
            let loader = sys::MFXLoad();
            if loader.is_null() {
                bail!("oneVPL MFXLoad returned null");
            }
            let fail = |loader: sys::mfxLoader, msg: String| -> anyhow::Error {
                sys::MFXUnload(loader);
                anyhow::anyhow!(msg)
            };

            let cfg_impl = sys::MFXCreateConfig(loader);
            if cfg_impl.is_null() {
                return Err(fail(loader, "MFXCreateConfig (impl) returned null".into()));
            }
            let mut v: sys::mfxVariant = std::mem::zeroed();
            v.Type = sys::mfxVariantType_MFX_VARIANT_TYPE_U32;
            v.Data.U32 = sys::mfxImplType_MFX_IMPL_TYPE_HARDWARE;
            let st = sys::MFXSetConfigFilterProperty(
                cfg_impl,
                b"mfxImplDescription.Impl\0".as_ptr(),
                v,
            );
            if st != sys::mfxStatus_MFX_ERR_NONE {
                return Err(fail(loader, format!("set impl filter: status {st}")));
            }

            let cfg_drm = sys::MFXCreateConfig(loader);
            if cfg_drm.is_null() {
                return Err(fail(loader, "MFXCreateConfig (drm) returned null".into()));
            }
            let mut dv: sys::mfxVariant = std::mem::zeroed();
            dv.Type = sys::mfxVariantType_MFX_VARIANT_TYPE_U32;
            dv.Data.U32 = render_node;
            let st = sys::MFXSetConfigFilterProperty(
                cfg_drm,
                b"mfxExtendedDeviceId.DRMRenderNodeNum\0".as_ptr(),
                dv,
            );
            if st != sys::mfxStatus_MFX_ERR_NONE {
                return Err(fail(loader, format!("set DRM-node filter: status {st}")));
            }

            let mut session: sys::mfxSession = std::ptr::null_mut();
            let st = sys::MFXCreateSession(loader, 0, &mut session);
            if st != sys::mfxStatus_MFX_ERR_NONE {
                return Err(fail(
                    loader,
                    format!("MFXCreateSession (DRM node {render_node}): status {st}"),
                ));
            }

            // --- mfxVideoParam: AV1 Main, P010 10-bit, CQP, no B-frames ---
            let mut vp: sys::mfxVideoParam = std::mem::zeroed();
            vp.IOPattern = sys::MFX_IOPATTERN_IN_SYSTEM_MEMORY as u16;
            vp.AsyncDepth = 1;

            let (fr_n, fr_d) = frame_rate_rational(config.frame_rate);
            let mut fi: sys::mfxFrameInfo = std::mem::zeroed();
            fi.FourCC = sys::MFX_FOURCC_P010;
            fi.ChromaFormat = sys::MFX_CHROMAFORMAT_YUV420 as u16;
            fi.BitDepthLuma = 10;
            fi.BitDepthChroma = 10;
            fi.Shift = 1; // P010: 10-bit samples in the high bits — REQUIRED.
            fi.PicStruct = sys::MFX_PICSTRUCT_PROGRESSIVE as u16;
            fi.FrameRateExtN = fr_n;
            fi.FrameRateExtD = fr_d;
            {
                let d = &mut fi.__bindgen_anon_1.__bindgen_anon_1;
                d.Width = aligned_w as u16;
                d.Height = aligned_h as u16;
                d.CropW = config.width as u16;
                d.CropH = config.height as u16;
            }

            {
                let mfx = &mut vp.__bindgen_anon_1.mfx;
                mfx.FrameInfo = fi;
                mfx.CodecId = sys::MFX_CODEC_AV1;
                mfx.CodecProfile = sys::MFX_PROFILE_AV1_MAIN as u16;
                let enc = &mut mfx.__bindgen_anon_1.__bindgen_anon_1;
                enc.TargetUsage = tp.target_usage;
                enc.GopPicSize = config.keyframe_interval.clamp(1, u16::MAX as u32) as u16;
                enc.GopRefDist = 1; // no B-frames
                enc.RateControlMethod = sys::MFX_RATECONTROL_CQP as u16;
                enc.__bindgen_anon_1.QPI = tp.qp_i;
                enc.__bindgen_anon_2.QPP = tp.qp_p;
                enc.__bindgen_anon_3.QPB = tp.qp_p;
            }

            // mfxExtCodingOption3: pin the *target* output bit depth to 10.
            let mut co3: sys::mfxExtCodingOption3 = std::mem::zeroed();
            co3.Header.BufferId = sys::MFX_EXTBUFF_CODING_OPTION3;
            co3.Header.BufferSz = std::mem::size_of::<sys::mfxExtCodingOption3>() as u32;
            co3.TargetBitDepthLuma = 10;
            co3.TargetBitDepthChroma = 10;
            co3.TargetChromaFormatPlus1 = (sys::MFX_CHROMAFORMAT_YUV420 + 1) as u16;
            let mut ext_bufs: [*mut sys::mfxExtBuffer; 1] =
                [&mut co3 as *mut sys::mfxExtCodingOption3 as *mut sys::mfxExtBuffer];
            vp.ExtParam = ext_bufs.as_mut_ptr();
            vp.NumExtParam = 1;

            // Query (tolerate warnings) then Init.
            let mut vp_out = vp;
            let st = sys::MFXVideoENCODE_Query(session, &mut vp, &mut vp_out);
            if st < 0 {
                return Err(fail(loader, format!("MFXVideoENCODE_Query: status {st}")));
            }
            let st = sys::MFXVideoENCODE_Init(session, &mut vp);
            if st < 0 {
                sys::MFXVideoENCODE_Close(session);
                sys::MFXClose(session);
                return Err(fail(loader, format!("MFXVideoENCODE_Init: status {st}")));
            }
            // ExtParam was copied by Init; drop our local pointer.
            vp.ExtParam = std::ptr::null_mut();
            vp.NumExtParam = 0;

            let bitstream_len = (aligned_w as usize) * (aligned_h as usize) * 4;
            Ok(Self {
                width: config.width,
                height: config.height,
                config,
                loader,
                session,
                bitstream_buf: vec![0u8; bitstream_len],
                collected: VecDeque::new(),
                frame_counter: 0,
                flushed: false,
            })
        }
    }

    /// Fill an acquired (mapped) P010 surface from a `Yuv420p10le` frame:
    /// Y then interleaved UV, each 10-bit sample in the high bits (`<< 6`),
    /// honoring the surface pitch.
    unsafe fn fill_p010(&self, surface: *mut sys::mfxFrameSurface1, frame: &VideoFrame) {
        unsafe {
            let data = &(*surface).Data;
            let pitch = data.__bindgen_anon_2.Pitch as usize;
            let y_ptr = data.__bindgen_anon_3.Y;
            let uv_ptr = data.__bindgen_anon_4.UV;
            let (w, h) = (self.width as usize, self.height as usize);
            let (cw, ch) = (w / 2, h / 2);
            let src = &frame.data;
            let rd = |off: usize| u16::from_le_bytes([src[off], src[off + 1]]) << 6;

            for row in 0..h {
                let dst = y_ptr.add(row * pitch);
                for col in 0..w {
                    let b = rd((row * w + col) * 2).to_le_bytes();
                    *dst.add(col * 2) = b[0];
                    *dst.add(col * 2 + 1) = b[1];
                }
            }
            let y_bytes = w * h * 2;
            let c_samples = cw * ch;
            let (u_off, v_off) = (y_bytes, y_bytes + c_samples * 2);
            for row in 0..ch {
                let dst = uv_ptr.add(row * pitch);
                for col in 0..cw {
                    let idx = row * cw + col;
                    let ub = rd(u_off + idx * 2).to_le_bytes();
                    let vb = rd(v_off + idx * 2).to_le_bytes();
                    *dst.add(col * 4) = ub[0];
                    *dst.add(col * 4 + 1) = ub[1];
                    *dst.add(col * 4 + 2) = vb[0];
                    *dst.add(col * 4 + 3) = vb[1];
                }
            }
        }
    }

    /// Submit one frame (`None` = drain). Returns the syncp if a frame is ready.
    unsafe fn encode_async(
        &mut self,
        surface: *mut sys::mfxFrameSurface1,
    ) -> Result<Option<sys::mfxSyncPoint>> {
        unsafe {
            let mut bs: sys::mfxBitstream = std::mem::zeroed();
            bs.Data = self.bitstream_buf.as_mut_ptr();
            bs.MaxLength = self.bitstream_buf.len() as u32;
            loop {
                let mut syncp: sys::mfxSyncPoint = std::ptr::null_mut();
                let st = sys::MFXVideoENCODE_EncodeFrameAsync(
                    self.session,
                    std::ptr::null_mut(),
                    surface,
                    &mut bs,
                    &mut syncp,
                );
                if st == sys::mfxStatus_MFX_WRN_DEVICE_BUSY {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    continue;
                }
                if st == sys::mfxStatus_MFX_ERR_MORE_DATA {
                    return Ok(None);
                }
                check(st, "MFXVideoENCODE_EncodeFrameAsync")?;
                // Sync and collect the produced bitstream.
                if !syncp.is_null() {
                    let st =
                        sys::MFXVideoCORE_SyncOperation(self.session, syncp, sys::MFX_INFINITE);
                    check(st, "MFXVideoCORE_SyncOperation")?;
                    let off = bs.DataOffset as usize;
                    let len = bs.DataLength as usize;
                    if len > 0 && off + len <= self.bitstream_buf.len() {
                        let is_kf = (bs.FrameType
                            & (sys::MFX_FRAMETYPE_IDR | sys::MFX_FRAMETYPE_I) as u16)
                            != 0;
                        self.collected.push_back(EncodedPacket {
                            data: Bytes::copy_from_slice(&self.bitstream_buf[off..off + len]),
                            pts: bs.TimeStamp,
                            is_keyframe: is_kf,
                        });
                    }
                }
                return Ok(Some(std::ptr::null_mut()));
            }
        }
    }
}

impl Encoder for QsvP010Encoder {
    fn send_frame(&mut self, frame: &VideoFrame) -> Result<()> {
        if frame.format != PixelFormat::Yuv420p10le {
            bail!("QsvP010Encoder expects Yuv420p10le, got {:?}", frame.format);
        }
        if frame.width != self.width || frame.height != self.height {
            bail!(
                "QsvP010Encoder fixed at {}×{}, received {}×{}",
                self.width,
                self.height,
                frame.width,
                frame.height
            );
        }
        let need = (self.width as usize * self.height as usize) * 3; // Yuv420p10le bytes
        if frame.data.len() < need {
            bail!("QsvP010Encoder: 10-bit frame too small: have {} need {}", frame.data.len(), need);
        }
        unsafe {
            let mut surface: *mut sys::mfxFrameSurface1 = std::ptr::null_mut();
            check(
                sys::MFXMemory_GetSurfaceForEncode(self.session, &mut surface),
                "MFXMemory_GetSurfaceForEncode",
            )?;
            if surface.is_null() {
                bail!("MFXMemory_GetSurfaceForEncode returned a null surface");
            }
            let iface = frame_interface(surface);
            if iface.is_null() {
                bail!("surface has no FrameInterface");
            }
            // Map → fill → Unmap.
            let map = (*iface).Map.ok_or_else(|| anyhow::anyhow!("FrameInterface.Map null"))?;
            check(map(surface, sys::mfxMemoryFlags_MFX_MAP_WRITE), "FrameInterface::Map")?;
            (*surface).Data.TimeStamp = self.frame_counter;
            self.fill_p010(surface, frame);
            if let Some(unmap) = (*iface).Unmap {
                check(unmap(surface), "FrameInterface::Unmap")?;
            }
            let r = self.encode_async(surface);
            // Release our reference to the surface (the encoder AddRef'd if kept).
            if let Some(release) = (*iface).Release {
                release(surface);
            }
            r?;
        }
        self.frame_counter += 1;
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if self.flushed {
            return Ok(());
        }
        self.flushed = true;
        // Drain: encode with a null surface until MFX_ERR_MORE_DATA (→ None).
        unsafe {
            while self.encode_async(std::ptr::null_mut())?.is_some() {}
        }
        Ok(())
    }

    fn receive_packet(&mut self) -> Result<Option<EncodedPacket>> {
        Ok(self.collected.pop_front())
    }
}

impl Drop for QsvP010Encoder {
    fn drop(&mut self) {
        unsafe {
            sys::MFXVideoENCODE_Close(self.session);
            sys::MFXClose(self.session);
            sys::MFXUnload(self.loader);
        }
    }
}

fn frame_rate_rational(fps: f64) -> (u32, u32) {
    if fps.fract().abs() < 1e-6 {
        (fps.round().max(1.0) as u32, 1)
    } else {
        ((fps * 1000.0).round().max(1.0) as u32, 1000)
    }
}
