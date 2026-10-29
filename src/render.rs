//! Affichage GPU : les plans Y, U et V décodés sont envoyés tels quels au GPU et
//! convertis en RGB dans le shader (pas de conversion CPU, mise à l'échelle gratuite).

use std::sync::Arc;

use anyhow::Context;
use winit::window::Window;

/// Image décodée, plans I420 sans padding.
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    /// Horodatage pour la mesure de latence.
    pub timing: Option<crate::latency::FrameTiming>,
}

/// Épaisseur du liseré du mode contrôle, en pixels.
pub const BORDER_WIDTH: f32 = 4.0;

struct Planes {
    width: u32,
    height: u32,
    y: wgpu::Texture,
    u: wgpu::Texture,
    v: wgpu::Texture,
    bind_group: wgpu::BindGroup,
}

pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    /// Cadre orange du mode contrôle (couleur unie).
    border_pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    planes: Option<Planes>,
}

const SHADER: &str = r#"
struct VsOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };

@vertex
fn vs(@builtin(vertex_index) i: u32) -> VsOut {
    // Triangle plein écran.
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    var o: VsOut;
    o.pos = vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0);
    o.uv = vec2<f32>(p.x, 1.0 - p.y);
    return o;
}

@group(0) @binding(0) var ty: texture_2d<f32>;
@group(0) @binding(1) var tu: texture_2d<f32>;
@group(0) @binding(2) var tv: texture_2d<f32>;
@group(0) @binding(3) var s: sampler;

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    // BT.601, plage limitée (16-235), comme produit par l'émetteur.
    let y = (textureSample(ty, s, in.uv).r - 0.0627451) * 1.164384;
    let u = textureSample(tu, s, in.uv).r - 0.5;
    let v = textureSample(tv, s, in.uv).r - 0.5;
    let rgb = vec3<f32>(y + 1.596027 * v, y - 0.391762 * u - 0.812968 * v, y + 2.017232 * u);
    return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
"#;

/// Couleur unie (cadre du mode contrôle), dessinée avec le même triangle plein écran.
const BORDER_SHADER: &str = r#"
@vertex
fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0);
}

@fragment
fn fs() -> @location(0) vec4<f32> {
    return vec4<f32>(1.0, 0.45, 0.0, 1.0);
}
"#;

impl Renderer {
    pub fn new(window: Arc<Window>) -> anyhow::Result<Self> {
        let size = window.inner_size();
        // DX12 en priorité : swapchain « flip model » composée par DWM, compatible avec
        // l'opacité de fenêtre ; Vulkan/GL en secours.
        let mut found = None;
        for backends in [wgpu::Backends::DX12, wgpu::Backends::all()] {
            let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
            desc.backends = backends;
            let instance = wgpu::Instance::new(desc);
            let surface = instance.create_surface(window.clone()).context("création de la surface")?;
            let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                ..Default::default()
            }));
            if let Ok(adapter) = adapter {
                found = Some((surface, adapter));
                break;
            }
        }
        let (surface, adapter) = found.context("aucun GPU compatible")?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: None,
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
            ..Default::default()
        }))
        .context("création du device GPU")?;

        let caps = surface.get_capabilities(&adapter);
        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .context("surface non supportée par le GPU")?;
        // Format non-sRGB : les valeurs YUV->RGB sont déjà en espace gamma.
        if let Some(f) = caps.formats.iter().find(|f| !f.is_srgb()) {
            config.format = *f;
        }
        // Mailbox = pas de déchirement et pas d'attente du vsync ; sinon Immediate.
        config.present_mode = [wgpu::PresentMode::Mailbox, wgpu::PresentMode::Immediate]
            .into_iter()
            .find(|m| caps.present_modes.contains(m))
            .unwrap_or(wgpu::PresentMode::Fifo);
        config.desired_maximum_frame_latency = 1;
        surface.configure(&device, &config);
        eprintln!("GPU : {} ({:?}, {:?})", adapter.get_info().name, adapter.get_info().backend, config.present_mode);

        let texture_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("yuv"),
            entries: &[
                texture_entry(0),
                texture_entry(1),
                texture_entry(2),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("yuv"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("yuv"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(config.format.into())],
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });
        let border_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("cadre"),
            source: wgpu::ShaderSource::Wgsl(BORDER_SHADER.into()),
        });
        let border_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[],
            immediate_size: 0,
        });
        let border_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("cadre"),
            layout: Some(&border_layout),
            vertex: wgpu::VertexState {
                module: &border_shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &border_shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(config.format.into())],
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        Ok(Self { surface, device, queue, config, pipeline, border_pipeline, layout, sampler, planes: None })
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
    }

    /// Taille de la dernière image reçue.
    pub fn video_size(&self) -> Option<(u32, u32)> {
        self.planes.as_ref().map(|p| (p.width, p.height))
    }

    pub fn upload(&mut self, f: &Frame) {
        if self.planes.as_ref().is_none_or(|p| p.width != f.width || p.height != f.height) {
            self.planes = Some(self.create_planes(f.width, f.height));
        }
        let p = self.planes.as_ref().unwrap();
        let write = |tex: &wgpu::Texture, data: &[u8], w: u32, h: u32| {
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: tex,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                data,
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w), rows_per_image: None },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            );
        };
        write(&p.y, &f.y, f.width, f.height);
        write(&p.u, &f.u, f.width / 2, f.height / 2);
        write(&p.v, &f.v, f.width / 2, f.height / 2);
    }

    fn create_planes(&self, width: u32, height: u32) -> Planes {
        let make = |w, h| {
            self.device.create_texture(&wgpu::TextureDescriptor {
                label: None,
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        };
        let (y, u, v) = (make(width, height), make(width / 2, height / 2), make(width / 2, height / 2));
        let view = |t: &wgpu::Texture| t.create_view(&Default::default());
        let (vy, vu, vv) = (view(&y), view(&u), view(&v));
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&vy) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&vu) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&vv) },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(&self.sampler) },
            ],
        });
        Planes { width, height, y, u, v, bind_group }
    }

    /// Zone de la fenêtre où l'image est dessinée (bandes noires exclues), en pixels.
    /// `inset` réserve une bordure (liseré du mode contrôle).
    pub fn video_rect(&self, keep_aspect: bool, inset: f32) -> Option<(f32, f32, f32, f32)> {
        let p = self.planes.as_ref()?;
        let (sw, sh) = (self.config.width as f32 - 2.0 * inset, self.config.height as f32 - 2.0 * inset);
        if sw < 1.0 || sh < 1.0 {
            return None;
        }
        let (mut x, mut y, mut w, mut h) = (0.0, 0.0, sw, sh);
        if keep_aspect {
            let scale = (sw / p.width as f32).min(sh / p.height as f32);
            w = (p.width as f32 * scale).round().max(1.0);
            h = (p.height as f32 * scale).round().max(1.0);
            x = ((sw - w) / 2.0).floor();
            y = ((sh - h) / 2.0).floor();
        }
        Some((x + inset, y + inset, w, h))
    }

    /// `border` : cadre orange de même épaisseur sur les 4 bords de la fenêtre (mode contrôle).
    pub fn render(&mut self, keep_aspect: bool, border: bool) {
        let inset = if border { BORDER_WIDTH } else { 0.0 };
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return;
            }
            _ => return,
        };
        let target = frame.texture.create_view(&Default::default());
        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if let (Some(p), Some((x, y, w, h))) = (&self.planes, self.video_rect(keep_aspect, inset)) {
                pass.set_viewport(x, y, w, h, 0.0, 1.0);
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &p.bind_group, &[]);
                pass.draw(0..3, 0..1);
            }
            if border {
                let (w, h, b) = (self.config.width as f32, self.config.height as f32, BORDER_WIDTH);
                pass.set_pipeline(&self.border_pipeline);
                // Haut, bas, gauche, droite.
                for (x, y, sw, sh) in [(0.0, 0.0, w, b), (0.0, h - b, w, b), (0.0, b, b, h - 2.0 * b), (w - b, b, b, h - 2.0 * b)] {
                    if sw >= 1.0 && sh >= 1.0 {
                        pass.set_viewport(x, y, sw, sh, 0.0, 1.0);
                        pass.draw(0..3, 0..1);
                    }
                }
            }
        }
        self.queue.submit([encoder.finish()]);
        self.queue.present(frame);
    }
}
