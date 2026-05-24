//! Per-CRTC gamma LUT helpers — atomic GAMMA_LUT property blob first,
//! legacy drmModeCrtcSetGamma ioctl as fallback. Used by the udev backend
//! to service the wlr-gamma-control protocol.

use std::iter::zip;
use std::num::NonZeroU64;
use std::os::fd::AsFd;

use smithay::reexports::drm::control::{self, Device as _, crtc, property};

use smithay::backend::drm::DrmDevice;

/// Per-CRTC atomic gamma property handles. Preferred over legacy
/// `drmModeCrtcSetGamma` ioctl — works under atomic modesetting, supports
/// deep-color LUTs, and can be reset cleanly via blob=0.
pub(crate) struct GammaProps {
    crtc: crtc::Handle,
    gamma_lut: control::property::Handle,
    gamma_lut_size: control::property::Handle,
    /// Holds the blob we last set, so we can destroy it after replacing.
    previous_blob: Option<NonZeroU64>,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct DrmColorLut {
    red: u16,
    green: u16,
    blue: u16,
    reserved: u16,
}

impl GammaProps {
    pub(crate) fn new(drm: &DrmDevice, crtc: crtc::Handle) -> Option<Self> {
        let mut gamma_lut = None;
        let mut gamma_lut_size = None;

        let props = drm.get_properties(crtc).ok()?;
        for (prop, _) in props {
            let Ok(info) = drm.get_property(prop) else {
                continue;
            };
            let Ok(name) = info.name().to_str() else {
                continue;
            };
            match name {
                "GAMMA_LUT" => {
                    if matches!(info.value_type(), property::ValueType::Blob) {
                        gamma_lut = Some(prop);
                    }
                }
                "GAMMA_LUT_SIZE" => {
                    if matches!(info.value_type(), property::ValueType::UnsignedRange(_, _)) {
                        gamma_lut_size = Some(prop);
                    }
                }
                _ => (),
            }
        }

        Some(Self {
            crtc,
            gamma_lut: gamma_lut?,
            gamma_lut_size: gamma_lut_size?,
            previous_blob: None,
        })
    }

    pub(crate) fn gamma_size(&self, drm: &DrmDevice) -> Option<u32> {
        let props = drm.get_properties(self.crtc).ok()?;
        for (prop, value) in props {
            if prop == self.gamma_lut_size {
                return Some(value as u32);
            }
        }
        None
    }

    /// True if a non-identity blob is currently set. Used on session resume
    /// to skip a no-op set_property(blob=0) ioctl for CRTCs no client has
    /// ever touched.
    pub(crate) fn has_previous_blob(&self) -> bool {
        self.previous_blob.is_some()
    }

    /// Apply `gamma` (R||G||B planar layout) atomically. `None` clears to
    /// identity by setting GAMMA_LUT blob = 0, which the kernel interprets
    /// as bypass.
    pub(crate) fn set_gamma(&mut self, drm: &DrmDevice, gamma: Option<&[u16]>) -> Option<()> {
        let blob_id = if let Some(gamma) = gamma {
            let size = self.gamma_size(drm)? as usize;
            if gamma.len() != size * 3 {
                tracing::warn!("wrong gamma length: got {}, expected {}", gamma.len(), size * 3);
                return None;
            }
            let (red, rest) = gamma.split_at(size);
            let (green, blue) = rest.split_at(size);
            let mut data: Vec<DrmColorLut> = zip(zip(red, green), blue)
                .map(|((&r, &g), &b)| DrmColorLut {
                    red: r,
                    green: g,
                    blue: b,
                    reserved: 0,
                })
                .collect();
            let raw = bytemuck::cast_slice_mut(&mut data);
            let blob = drm_ffi::mode::create_property_blob(drm.as_fd(), raw)
                .map_err(|e| tracing::warn!("failed to create GAMMA_LUT blob: {e:?}"))
                .ok()?;
            NonZeroU64::new(u64::from(blob.blob_id))
        } else {
            None
        };

        let raw_id = blob_id.map(NonZeroU64::get).unwrap_or(0);
        if let Err(e) =
            drm.set_property(self.crtc, self.gamma_lut, property::Value::Blob(raw_id).into())
        {
            tracing::warn!("failed to set GAMMA_LUT property: {e:?}");
            if raw_id != 0 {
                let _ = drm_ffi::mode::destroy_property_blob(drm.as_fd(), raw_id as u32);
            }
            return None;
        }

        if let Some(old) = std::mem::replace(&mut self.previous_blob, blob_id) {
            let _ = drm_ffi::mode::destroy_property_blob(drm.as_fd(), old.get() as u32);
        }

        Some(())
    }

    /// Re-apply the last-set blob without rebuilding it. Used on session
    /// resume — the kernel resets CRTC gamma to identity on VT switch, but
    /// our `previous_blob` handle stays valid because the DRM fd is paused,
    /// not closed.
    ///
    /// On failure, drops `previous_blob` so a subsequent `set_gamma` doesn't
    /// try to `destroy_property_blob` a kernel-side handle we may have lost.
    pub(crate) fn restore_gamma(&mut self, drm: &DrmDevice) -> Option<()> {
        let raw = self.previous_blob.map(NonZeroU64::get).unwrap_or(0);
        if let Err(e) =
            drm.set_property(self.crtc, self.gamma_lut, property::Value::Blob(raw).into())
        {
            tracing::warn!("failed to restore GAMMA_LUT: {e:?}");
            self.previous_blob = None;
            return None;
        }
        Some(())
    }
}

/// Legacy fallback: drmModeCrtcSetGamma ioctl. Called when GAMMA_LUT
/// property isn't present (older drivers, some virtual devices). The legacy
/// ioctl has no "reset" — we generate a linear identity ramp manually.
pub(crate) fn set_gamma_for_crtc_legacy(
    drm: &DrmDevice,
    crtc: crtc::Handle,
    ramp: Option<&[u16]>,
) -> Option<()> {
    let info = drm.get_crtc(crtc).ok()?;
    let len = info.gamma_length() as usize;
    if len == 0 {
        tracing::warn!("legacy gamma not supported on this CRTC");
        return None;
    }

    let identity;
    let ramp = match ramp {
        Some(r) if r.len() == len * 3 => r,
        Some(r) => {
            tracing::warn!("wrong gamma length: got {}, expected {}", r.len(), len * 3);
            return None;
        }
        None => {
            // Legacy ioctl has no reset — fill an identity ramp manually.
            // `len > 0` ensured above; we trust `len > 1` (any real LUT).
            let mut buf = vec![0u16; len * 3];
            let (r_buf, rest) = buf.split_at_mut(len);
            let (g_buf, b_buf) = rest.split_at_mut(len);
            let denom = len as u64 - 1;
            for (i, ((r, g), b)) in zip(zip(r_buf, g_buf), b_buf).enumerate() {
                let v = ((0xFFFFu64 * i as u64) / denom) as u16;
                *r = v;
                *g = v;
                *b = v;
            }
            identity = buf;
            &identity
        }
    };
    let (red, rest) = ramp.split_at(len);
    let (green, blue) = rest.split_at(len);
    drm.set_gamma(crtc, red, green, blue)
        .map_err(|e| tracing::warn!("legacy set_gamma failed: {e:?}"))
        .ok()?;
    Some(())
}
