//! Screen capture with DXGI Desktop Duplication: Windows hands us the
//! monitor's picture (only when something changed) plus the mouse pointer.
//! Nothing here exists until you go live; dropping the `Capturer` frees the
//! GPU copy and the device.

#[allow(unused_imports)]
use crate::convert::Pointer;

/// A monitor you can share.
#[derive(Clone, Debug)]
pub struct Monitor {
    pub name: String,
    pub w: u32,
    pub h: u32,
    pub primary: bool,
}

#[derive(Debug)]
pub enum CapErr {
    /// The picture went away for a moment (resolution change, UAC prompt,
    /// lock screen, a game switching modes): make a new `Capturer`.
    Lost,
    Failed(String),
}

#[cfg(windows)]
pub use imp::{monitors, Capturer};

/// Which monitor a saved choice means: out of range (or "main", usize::MAX)
/// picks the main screen.
pub fn resolve(index: usize, list: &[Monitor]) -> usize {
    if index < list.len() {
        index
    } else {
        list.iter().position(|m| m.primary).unwrap_or(0)
    }
}

#[cfg(windows)]
mod imp {
    use super::{CapErr, Monitor, Pointer};
    use windows::core::Interface;
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::*;
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Graphics::Dxgi::Common::*;
    use windows::Win32::Graphics::Dxgi::*;

    fn outputs() -> Vec<(IDXGIAdapter1, IDXGIOutput, DXGI_OUTPUT_DESC)> {
        let mut v = Vec::new();
        unsafe {
            let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else { return v };
            let mut a = 0;
            while let Ok(adapter) = factory.EnumAdapters1(a) {
                let mut o = 0;
                while let Ok(out) = adapter.EnumOutputs(o) {
                    if let Ok(desc) = out.GetDesc() {
                        if desc.AttachedToDesktop.as_bool() {
                            v.push((adapter.clone(), out, desc));
                        }
                    }
                    o += 1;
                }
                a += 1;
            }
        }
        v
    }

    pub fn monitors() -> Vec<Monitor> {
        outputs()
            .iter()
            .enumerate()
            .map(|(i, (_, _, d))| {
                let r = d.DesktopCoordinates;
                let primary = r.left == 0 && r.top == 0;
                Monitor {
                    name: format!("Screen {}{}", i + 1, if primary { " (main)" } else { "" }),
                    w: (r.right - r.left).max(0) as u32,
                    h: (r.bottom - r.top).max(0) as u32,
                    primary,
                }
            })
            .collect()
    }

    pub struct Capturer {
        ctx: ID3D11DeviceContext,
        device: ID3D11Device,
        dup: IDXGIOutputDuplication,
        /// CPU-readable copies of the screen. Copies come back from the GPU
        /// a little later; we never wait for one (a game keeping the GPU
        /// busy would stall the stream's clock), we use the newest that's done.
        staging: Vec<ID3D11Texture2D>,
        size: (usize, usize),
        /// Copies on their way back, oldest first (indices into `staging`).
        pending: std::collections::VecDeque<usize>,
        /// The copy used last (complete; reused when only the pointer moves).
        shown: Option<usize>,
        pointer: Pointer,
        ptr_pos: (i32, i32),
        ptr_visible: bool,
    }

    // The D3D objects are only ever used from the capture thread that made
    // them; the struct is moved there once.
    unsafe impl Send for Capturer {}

    impl Capturer {
        pub fn new(index: usize) -> Result<Capturer, CapErr> {
            let mut list = outputs();
            if list.is_empty() {
                return Err(CapErr::Failed("No screen found".into()));
            }
            let pick = super::resolve(index, &monitors());
            let (adapter, output, _) = list.swap_remove(pick);
            unsafe {
                let mut device = None;
                let mut ctx = None;
                D3D11CreateDevice(
                    &adapter,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    Some(&[D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_10_0]),
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut ctx),
                )
                .map_err(|e| CapErr::Failed(format!("Graphics device: {e}")))?;
                let (Some(device), Some(ctx)) = (device, ctx) else {
                    return Err(CapErr::Failed("Graphics device missing".into()));
                };
                let out1: IDXGIOutput1 = output.cast().map_err(|e| CapErr::Failed(format!("{e}")))?;
                let dup = out1.DuplicateOutput(&device).map_err(|e| {
                    if e.code() == DXGI_ERROR_NOT_CURRENTLY_AVAILABLE {
                        CapErr::Failed("Too many apps are capturing the screen".into())
                    } else if e.code() == DXGI_ERROR_UNSUPPORTED {
                        CapErr::Failed("This screen can't be captured (unsupported)".into())
                    } else {
                        // E_ACCESSDENIED on the lock screen / UAC prompt: retry.
                        CapErr::Lost
                    }
                })?;
                Ok(Capturer {
                    ctx,
                    device,
                    dup,
                    staging: Vec::new(),
                    size: (0, 0),
                    pending: std::collections::VecDeque::new(),
                    shown: None,
                    pointer: Pointer::default(),
                    ptr_pos: (0, 0),
                    ptr_visible: false,
                })
            }
        }

        /// Wait up to `timeout_ms` for the screen to change. Ok(true) means
        /// the picture or the pointer changed.
        pub fn next(&mut self, timeout_ms: u32) -> Result<bool, CapErr> {
            unsafe {
                let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
                let mut res: Option<IDXGIResource> = None;
                if let Err(e) = self.dup.AcquireNextFrame(timeout_ms, &mut info, &mut res) {
                    return if e.code() == DXGI_ERROR_WAIT_TIMEOUT { Ok(false) } else { Err(CapErr::Lost) };
                }
                let mut changed = false;
                if info.LastPresentTime != 0 {
                    if let Some(tex) = res.as_ref().and_then(|r| r.cast::<ID3D11Texture2D>().ok()) {
                        let mut d = D3D11_TEXTURE2D_DESC::default();
                        tex.GetDesc(&mut d);
                        let (w, h) = (d.Width as usize, d.Height as usize);
                        if self.staging.len() != 3 || self.size != (w, h) {
                            self.staging.clear();
                            self.pending.clear();
                            self.shown = None;
                            self.size = (w, h);
                            let sd = D3D11_TEXTURE2D_DESC {
                                Width: d.Width,
                                Height: d.Height,
                                MipLevels: 1,
                                ArraySize: 1,
                                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                                Usage: D3D11_USAGE_STAGING,
                                BindFlags: 0,
                                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                                MiscFlags: 0,
                            };
                            for _ in 0..3 {
                                let mut t = None;
                                if self.device.CreateTexture2D(&sd, None, Some(&mut t)).is_ok() {
                                    if let Some(t) = t {
                                        self.staging.push(t);
                                    }
                                }
                            }
                            if self.staging.len() != 3 {
                                self.staging.clear();
                            }
                        }
                        if !self.staging.is_empty() {
                            // A free copy slot, or else the oldest copy still on its way.
                            let free = (0..3).find(|i| Some(*i) != self.shown && !self.pending.contains(i));
                            let idx = free.or_else(|| self.pending.pop_front()).unwrap_or(0);
                            self.ctx.CopyResource(&self.staging[idx], &tex);
                            self.ctx.Flush(); // start the copy now
                            self.pending.retain(|i| *i != idx);
                            self.pending.push_back(idx);
                            changed = true;
                        }
                    }
                }
                if info.LastMouseUpdateTime != 0 {
                    let vis = info.PointerPosition.Visible.as_bool();
                    let pos = (info.PointerPosition.Position.x, info.PointerPosition.Position.y);
                    if vis != self.ptr_visible || (vis && pos != self.ptr_pos) {
                        changed = true;
                    }
                    self.ptr_visible = vis;
                    self.ptr_pos = pos;
                }
                if info.PointerShapeBufferSize > 0 {
                    let mut buf = vec![0u8; info.PointerShapeBufferSize as usize];
                    let mut needed = 0u32;
                    let mut si = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
                    if self
                        .dup
                        .GetFramePointerShape(buf.len() as u32, buf.as_mut_ptr() as _, &mut needed, &mut si)
                        .is_ok()
                    {
                        self.pointer = Pointer {
                            kind: si.Type,
                            w: si.Width as usize,
                            h: si.Height as usize,
                            pitch: si.Pitch as usize,
                            data: buf,
                        };
                        changed = true;
                    }
                }
                drop(res);
                let _ = self.dup.ReleaseFrame();
                Ok(changed)
            }
        }

        /// A copy is still on its way back from the GPU.
        pub fn has_pending(&self) -> bool {
            !self.pending.is_empty()
        }

        /// Run `f` on the newest finished picture: (BGRA pixels, width,
        /// height, bytes per row, the pointer and where it is). None when the
        /// newest copy isn't back yet (try again next tick).
        pub fn with_frame<R>(
            &mut self,
            f: impl FnOnce(&[u8], usize, usize, usize, Option<(&Pointer, i32, i32)>) -> R,
        ) -> Option<R> {
            let (w, h) = self.size;
            unsafe {
                let mut m = D3D11_MAPPED_SUBRESOURCE::default();
                // Newest finished copy, without waiting for the GPU.
                let mut found = None;
                for k in (0..self.pending.len()).rev() {
                    let idx = self.pending[k];
                    let flags = D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32;
                    if self.ctx.Map(&self.staging[idx], 0, D3D11_MAP_READ, flags, Some(&mut m)).is_ok() {
                        found = Some((k, idx));
                        break;
                    }
                }
                let idx = match found {
                    Some((k, idx)) => {
                        self.pending.drain(..=k); // older copies are outdated
                        self.shown = Some(idx);
                        idx
                    }
                    // Only the pointer moved: redraw the last picture.
                    None if self.pending.is_empty() => {
                        let idx = self.shown?;
                        self.ctx.Map(&self.staging[idx], 0, D3D11_MAP_READ, 0, Some(&mut m)).ok()?;
                        idx
                    }
                    None => return None,
                };
                let pitch = m.RowPitch as usize;
                let data = std::slice::from_raw_parts(m.pData as *const u8, pitch * h);
                let r = f(data, w, h, pitch, self.pointer());
                self.ctx.Unmap(&self.staging[idx], 0);
                Some(r)
            }
        }

        /// The pointer and its top-left position on this screen, if it's here.
        pub fn pointer(&self) -> Option<(&Pointer, i32, i32)> {
            (self.ptr_visible && self.pointer.w > 0).then_some((&self.pointer, self.ptr_pos.0, self.ptr_pos.1))
        }
    }
}

/// Other systems (development builds) have no screens to share.
#[cfg(not(windows))]
pub fn monitors() -> Vec<Monitor> {
    Vec::new()
}

/// A moving test picture, so the rest of the pipeline can run in tests.
#[cfg(not(windows))]
pub struct Capturer {
    w: usize,
    h: usize,
    t: usize,
    buf: Vec<u8>,
}

#[cfg(not(windows))]
impl Capturer {
    pub fn new(_index: usize) -> Result<Capturer, CapErr> {
        let (w, h) = (1280, 720);
        Ok(Capturer { w, h, t: 0, buf: vec![0; w * h * 4] })
    }

    pub fn next(&mut self, timeout_ms: u32) -> Result<bool, CapErr> {
        std::thread::sleep(std::time::Duration::from_millis(timeout_ms.min(16) as u64));
        self.t += 1;
        let (w, h, t) = (self.w, self.h, self.t);
        for y in 0..h {
            for x in 0..w {
                let o = (y * w + x) * 4;
                let v = ((x + t * 4) ^ y) as u8;
                self.buf[o..o + 4].copy_from_slice(&[v, v.wrapping_add(80), 255 - v, 255]);
            }
        }
        Ok(true)
    }

    pub fn has_pending(&self) -> bool {
        false
    }

    pub fn with_frame<R>(&mut self, f: impl FnOnce(&[u8], usize, usize, usize, Option<(&Pointer, i32, i32)>) -> R) -> Option<R> {
        Some(f(&self.buf, self.w, self.h, self.w * 4, None))
    }

    pub fn pointer(&self) -> Option<(&Pointer, i32, i32)> {
        None
    }
}
