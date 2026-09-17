//! The video layer drawn underneath the egui chrome on Linux.
//!
//! Frames arrive as CPU RGBA (the software decoder's output, already converted
//! by `DecodedSurface::prepare`), so this is one upload and one textured quad.
//! The quad is letterboxed into the content area and can be rotated by the
//! multiples of 90° the remote display reports.
use anyhow::{Context, Result};
use wgpu::util::DeviceExt;

/// Where the video sits inside the window, in physical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct VideoPlacement {
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) width: f32,
    pub(crate) height: f32,
    /// Clockwise rotation in degrees: 0, 90, 180 or 270.
    pub(crate) rotation: u16,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniform {
    /// Quad rect in clip space: offset then half-extent.
    offset: [f32; 2],
    extent: [f32; 2],
    /// Texture-space rotation matrix, column major.
    rotation: [f32; 4],
}

pub(crate) struct VideoLayer {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    uniform: wgpu::Buffer,
    texture: Option<(wgpu::Texture, u32, u32)>,
    bind_group: Option<wgpu::BindGroup>,
}

impl VideoLayer {
    pub(crate) fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("openuuyc-video"),
            source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(SHADER)),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("openuuyc-video"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("openuuyc-video"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("openuuyc-video"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let uniform = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("openuuyc-video"),
            contents: bytemuck::bytes_of(&Uniform {
                offset: [0.0, 0.0],
                extent: [1.0, 1.0],
                rotation: [1.0, 0.0, 0.0, 1.0],
            }),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("openuuyc-video"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        Self {
            pipeline,
            layout,
            sampler,
            uniform,
            texture: None,
            bind_group: None,
        }
    }

    pub(crate) fn has_frame(&self) -> bool {
        self.texture.is_some()
    }

    pub(crate) fn clear(&mut self) {
        self.texture = None;
        self.bind_group = None;
    }

    /// `pixels` is tightly packed RGBA8, `width * height` entries.
    pub(crate) fn upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        width: u32,
        height: u32,
        pixels: &[u8],
    ) -> Result<()> {
        anyhow::ensure!(
            width > 0 && height > 0 && pixels.len() >= (width as usize * height as usize * 4),
            "视频帧尺寸与数据不匹配"
        );
        let reuse = matches!(self.texture, Some((_, w, h)) if w == width && h == height);
        if !reuse {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("openuuyc-video"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            self.bind_group = Some(device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("openuuyc-video"),
                layout: &self.layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: self.uniform.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                ],
            }));
            self.texture = Some((texture, width, height));
        }
        let (texture, _, _) = self.texture.as_ref().context("视频纹理不可用")?;
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &pixels[..width as usize * height as usize * 4],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width * 4),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        Ok(())
    }

    pub(crate) fn draw(
        &self,
        queue: &wgpu::Queue,
        pass: &mut wgpu::RenderPass<'static>,
        target: (u32, u32),
        placement: VideoPlacement,
    ) {
        let (Some(bind_group), true) = (self.bind_group.as_ref(), target.0 > 0 && target.1 > 0)
        else {
            return;
        };
        let width = target.0 as f32;
        let height = target.1 as f32;
        // Clip space is [-1, 1] with y up; the placement is in pixels with y down.
        let center_x = (placement.x + placement.width / 2.0) / width * 2.0 - 1.0;
        let center_y = 1.0 - (placement.y + placement.height / 2.0) / height * 2.0;
        let rotation = match placement.rotation % 360 {
            90 => [0.0, 1.0, -1.0, 0.0],
            180 => [-1.0, 0.0, 0.0, -1.0],
            270 => [0.0, -1.0, 1.0, 0.0],
            _ => [1.0, 0.0, 0.0, 1.0],
        };
        queue.write_buffer(
            &self.uniform,
            0,
            bytemuck::bytes_of(&Uniform {
                offset: [center_x, center_y],
                extent: [
                    placement.width / width,
                    placement.height / height,
                ],
                rotation,
            }),
        );
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, bind_group, &[]);
        pass.draw(0..4, 0..1);
    }
}

const SHADER: &str = r#"
struct Uniform {
    offset: vec2<f32>,
    extent: vec2<f32>,
    rotation: vec4<f32>,
};

@group(0) @binding(0) var<uniform> settings: Uniform;
@group(0) @binding(1) var frame: texture_2d<f32>;
@group(0) @binding(2) var frame_sampler: sampler;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VertexOutput {
    // Triangle strip over the unit quad: (0,0) (1,0) (0,1) (1,1).
    let corner = vec2<f32>(f32(index & 1u), f32((index >> 1u) & 1u));
    // Clip space has y up while the placement and the texture have y down.
    let centered = vec2<f32>(corner.x * 2.0 - 1.0, 1.0 - corner.y * 2.0);
    var out: VertexOutput;
    out.position = vec4<f32>(settings.offset + centered * settings.extent, 0.0, 1.0);
    // Rotate around the texture centre, then move back into [0, 1].
    let matrix = mat2x2<f32>(
        settings.rotation.x, settings.rotation.y,
        settings.rotation.z, settings.rotation.w,
    );
    let rotated = matrix * (corner - vec2<f32>(0.5, 0.5));
    out.uv = rotated + vec2<f32>(0.5, 0.5);
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return textureSample(frame, frame_sampler, in.uv);
}
"#;
