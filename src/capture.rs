//! Capture d'écran via DXGI Desktop Duplication : c'est l'API la plus rapide de Windows,
//! elle donne directement la surface composée par DWM, sans copie GDI.

use std::fmt;

use anyhow::{Context, bail};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::Interface;

pub struct MonitorInfo {
    pub name: String,
    pub width: i32,
    pub height: i32,
    pub x: i32,
    pub y: i32,
    adapter: IDXGIAdapter1,
    output: IDXGIOutput1,
}

impl fmt::Display for MonitorInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}x{} à ({}, {})", self.name, self.width, self.height, self.x, self.y)
    }
}

pub fn list_monitors() -> anyhow::Result<Vec<MonitorInfo>> {
    let mut out = Vec::new();
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().context("CreateDXGIFactory1")?;
        let mut ai = 0;
        while let Ok(adapter) = factory.EnumAdapters1(ai) {
            let mut oi = 0;
            while let Ok(output) = adapter.EnumOutputs(oi) {
                let desc = output.GetDesc()?;
                if desc.AttachedToDesktop.as_bool() {
                    let r = desc.DesktopCoordinates;
                    let len = desc.DeviceName.iter().position(|&c| c == 0).unwrap_or(32);
                    out.push(MonitorInfo {
                        name: String::from_utf16_lossy(&desc.DeviceName[..len]),
                        width: r.right - r.left,
                        height: r.bottom - r.top,
                        x: r.left,
                        y: r.top,
                        adapter: adapter.clone(),
                        output: output.cast()?,
                    });
                }
                oi += 1;
            }
            ai += 1;
        }
    }
    Ok(out)
}

/// Forme du curseur telle que fournie par DXGI.
struct PointerShape {
    kind: i32,
    width: u32,
    height: u32,
    pitch: u32,
    data: Vec<u8>,
}

pub enum FrameStatus {
    /// Rien n'a changé (ni l'image, ni la souris) pendant le délai.
    Unchanged,
    /// Nouvelle image disponible dans [`Capturer::frame`].
    Updated,
}

pub struct Capturer {
    monitor_index: usize,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    dupl: IDXGIOutputDuplication,
    staging: Option<ID3D11Texture2D>,
    pub width: usize,
    pub height: usize,
    /// Position et taille de l'écran capturé sur le bureau (x, y, largeur, hauteur).
    pub monitor_rect: (i32, i32, i32, i32),
    /// Image du bureau sans le curseur (BGRA, `width * 4` octets par ligne).
    clean: Vec<u8>,
    /// Image envoyée : bureau + curseur.
    pub frame: Vec<u8>,
    pointer: Option<PointerShape>,
    pointer_pos: Option<(i32, i32)>,
    draw_cursor: bool,
}

impl Capturer {
    pub fn new(monitor_index: usize, draw_cursor: bool) -> anyhow::Result<Self> {
        let monitors = list_monitors()?;
        let Some(mon) = monitors.into_iter().nth(monitor_index) else {
            bail!("écran {monitor_index} introuvable (voir `pip-pbp monitors`)");
        };
        eprintln!("Capture de l'écran {mon}");
        let (device, context, dupl) = unsafe { Self::open(&mon)? };
        let monitor_rect = (mon.x, mon.y, mon.width, mon.height);
        let mut c = Self {
            monitor_rect,
            monitor_index,
            device,
            context,
            dupl,
            staging: None,
            width: 0,
            height: 0,
            clean: Vec::new(),
            frame: Vec::new(),
            pointer: None,
            pointer_pos: None,
            draw_cursor,
        };
        c.update_size();
        Ok(c)
    }

    unsafe fn open(mon: &MonitorInfo) -> anyhow::Result<(ID3D11Device, ID3D11DeviceContext, IDXGIOutputDuplication)> {
        unsafe {
            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(
                &mon.adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .context("D3D11CreateDevice")?;
            let device: ID3D11Device = device.context("pas de device D3D11")?;
            let dupl = mon
                .output
                .DuplicateOutput(&device)
                .context("DuplicateOutput (déjà utilisé par une autre application ?)")?;
            Ok((device, context.context("pas de contexte D3D11")?, dupl))
        }
    }

    fn update_size(&mut self) {
        let desc = unsafe { self.dupl.GetDesc() };
        self.width = desc.ModeDesc.Width as usize;
        self.height = desc.ModeDesc.Height as usize;
        self.clean = vec![0; self.width * self.height * 4];
        self.frame = self.clean.clone();
        self.staging = None;
    }

    /// Recrée la duplication (changement de résolution, écran de verrouillage, UAC…).
    fn reopen(&mut self) -> anyhow::Result<()> {
        let monitors = list_monitors()?;
        let mon = monitors.into_iter().nth(self.monitor_index).context("écran disparu")?;
        let (device, context, dupl) = unsafe { Self::open(&mon)? };
        self.monitor_rect = (mon.x, mon.y, mon.width, mon.height);
        self.device = device;
        self.context = context;
        self.dupl = dupl;
        self.update_size();
        Ok(())
    }

    /// Attend une nouvelle image pendant au plus `timeout_ms`.
    pub fn next_frame(&mut self, timeout_ms: u32) -> anyhow::Result<FrameStatus> {
        match unsafe { self.acquire(timeout_ms) } {
            Ok(s) => Ok(s),
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => Ok(FrameStatus::Unchanged),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST || e.code() == DXGI_ERROR_INVALID_CALL => {
                eprintln!("Duplication perdue ({e}), réouverture…");
                std::thread::sleep(std::time::Duration::from_millis(100));
                self.reopen()?;
                Ok(FrameStatus::Unchanged)
            }
            Err(e) => Err(e.into()),
        }
    }

    unsafe fn acquire(&mut self, timeout_ms: u32) -> windows::core::Result<FrameStatus> {
        unsafe {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            self.dupl.AcquireNextFrame(timeout_ms, &mut info, &mut resource)?;
            let result = self.process(&info, resource);
            self.dupl.ReleaseFrame()?;
            result
        }
    }

    unsafe fn process(&mut self, info: &DXGI_OUTDUPL_FRAME_INFO, resource: Option<IDXGIResource>) -> windows::core::Result<FrameStatus> {
        unsafe {
            let mut changed = false;

            if info.LastPresentTime != 0
                && info.AccumulatedFrames > 0
                && let Some(resource) = resource
            {
                let tex: ID3D11Texture2D = resource.cast()?;
                self.copy_to_cpu(&tex)?;
                changed = true;
            }

            if info.LastMouseUpdateTime != 0 {
                let p = info.PointerPosition;
                let pos = p.Visible.as_bool().then_some((p.Position.x, p.Position.y));
                if pos != self.pointer_pos {
                    self.pointer_pos = pos;
                    changed = true;
                }
            }
            if info.PointerShapeBufferSize > 0 {
                let mut data = vec![0u8; info.PointerShapeBufferSize as usize];
                let mut required = 0;
                let mut shape = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
                self.dupl.GetFramePointerShape(
                    data.len() as u32,
                    data.as_mut_ptr().cast(),
                    &mut required,
                    &mut shape,
                )?;
                self.pointer = Some(PointerShape {
                    kind: shape.Type as i32,
                    width: shape.Width,
                    height: shape.Height,
                    pitch: shape.Pitch,
                    data,
                });
                changed = true;
            }

            if !changed {
                return Ok(FrameStatus::Unchanged);
            }
            self.frame.copy_from_slice(&self.clean);
            if self.draw_cursor {
                self.blend_cursor();
            }
            Ok(FrameStatus::Updated)
        }
    }

    unsafe fn copy_to_cpu(&mut self, tex: &ID3D11Texture2D) -> windows::core::Result<()> {
        unsafe {
            if self.staging.is_none() {
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                tex.GetDesc(&mut desc);
                desc.Usage = D3D11_USAGE_STAGING;
                desc.BindFlags = 0;
                desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
                desc.MiscFlags = 0;
                desc.MipLevels = 1;
                desc.ArraySize = 1;
                desc.SampleDesc = DXGI_SAMPLE_DESC { Count: 1, Quality: 0 };
                let mut staging = None;
                self.device.CreateTexture2D(&desc, None, Some(&mut staging))?;
                self.staging = staging;
            }
            let staging = self.staging.as_ref().unwrap();
            self.context.CopyResource(staging, tex);

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let pitch = mapped.RowPitch as usize;
            let row = self.width * 4;
            let src = std::slice::from_raw_parts(mapped.pData as *const u8, pitch * self.height);
            if pitch == row {
                self.clean.copy_from_slice(&src[..row * self.height]);
            } else {
                for (dst, s) in self.clean.chunks_exact_mut(row).zip(src.chunks(pitch)) {
                    dst.copy_from_slice(&s[..row]);
                }
            }
            self.context.Unmap(staging, 0);
            Ok(())
        }
    }

    /// Dessine le curseur dans `frame` (les 3 formats de curseur DXGI).
    fn blend_cursor(&mut self) {
        let (Some(shape), Some((px, py))) = (&self.pointer, self.pointer_pos) else { return };
        let (fw, fh) = (self.width as i32, self.height as i32);
        let monochrome = shape.kind == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0;
        let h = if monochrome { shape.height / 2 } else { shape.height } as i32;
        let w = shape.width as i32;

        for cy in 0..h {
            let y = py + cy;
            if y < 0 || y >= fh {
                continue;
            }
            for cx in 0..w {
                let x = px + cx;
                if x < 0 || x >= fw {
                    continue;
                }
                let d = ((y * fw + x) * 4) as usize;
                let dst = &mut self.frame[d..d + 4];
                if monochrome {
                    let bit = 0x80 >> (cx % 8);
                    let and_byte = shape.data[(cy as u32 * shape.pitch) as usize + (cx / 8) as usize];
                    let xor_byte = shape.data[((cy + h) as u32 * shape.pitch) as usize + (cx / 8) as usize];
                    let and_mask = if and_byte & bit != 0 { 0xFF } else { 0 };
                    let xor_mask = if xor_byte & bit != 0 { 0xFF } else { 0 };
                    for c in &mut dst[..3] {
                        *c = (*c & and_mask) ^ xor_mask;
                    }
                } else {
                    let s = (cy as u32 * shape.pitch + cx as u32 * 4) as usize;
                    let src = &shape.data[s..s + 4];
                    if shape.kind == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 {
                        if src[3] == 0 {
                            dst[..3].copy_from_slice(&src[..3]);
                        } else {
                            for i in 0..3 {
                                dst[i] ^= src[i];
                            }
                        }
                    } else {
                        let a = src[3] as u32;
                        for i in 0..3 {
                            dst[i] = ((src[i] as u32 * a + dst[i] as u32 * (255 - a)) / 255) as u8;
                        }
                    }
                }
            }
        }
    }
}
