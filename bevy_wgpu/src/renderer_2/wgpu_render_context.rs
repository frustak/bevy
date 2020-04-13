use super::WgpuRenderResourceContextTrait;
use crate::{
    wgpu_type_converter::{OwnedWgpuVertexBufferDescriptor, WgpuInto},
    WgpuRenderPass,
};
use bevy_asset::{AssetStorage, Handle};
use bevy_render::{
    pass::{
        PassDescriptor, RenderPass, RenderPassColorAttachmentDescriptor,
        RenderPassDepthStencilAttachmentDescriptor,
    },
    pipeline::{BindGroupDescriptor, BindType, PipelineDescriptor},
    render_resource::{
        resource_name, RenderResource, RenderResourceAssignments, RenderResourceSetId, ResourceInfo,
    },
    renderer_2::{RenderContext, RenderResourceContext},
    shader::Shader,
    texture::TextureDescriptor,
};
use bevy_window::WindowId;
use std::{collections::HashMap, sync::Arc};

#[derive(Default)]
struct LazyCommandEncoder {
    command_encoder: Option<wgpu::CommandEncoder>,
}

impl LazyCommandEncoder {
    pub fn get_or_create(&mut self, device: &wgpu::Device) -> &mut wgpu::CommandEncoder {
        match self.command_encoder {
            Some(ref mut command_encoder) => command_encoder,
            None => {
                self.create(device);
                self.command_encoder.as_mut().unwrap()
            }
        }
    }

    pub fn is_some(&self) -> bool {
        self.command_encoder.is_some()
    }

    pub fn create(&mut self, device: &wgpu::Device) {
        let command_encoder =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        self.command_encoder = Some(command_encoder);
    }

    pub fn take(&mut self) -> Option<wgpu::CommandEncoder> {
        self.command_encoder.take()
    }

    pub fn set(&mut self, command_encoder: wgpu::CommandEncoder) {
        self.command_encoder = Some(command_encoder);
    }
}

pub struct WgpuRenderContext<T>
where
    T: RenderResourceContext,
{
    pub device: Arc<wgpu::Device>,
    // TODO: remove this
    pub primary_window: Option<WindowId>,
    command_encoder: LazyCommandEncoder,
    pub render_resources: T,
}

impl<T> WgpuRenderContext<T>
where
    T: RenderResourceContext,
{
    pub fn new(device: Arc<wgpu::Device>, resources: T) -> Self {
        WgpuRenderContext {
            device,
            primary_window: None,
            render_resources: resources,
            command_encoder: LazyCommandEncoder::default(),
        }
    }

    /// Consume this context, finalize the current CommandEncoder (if it exists), and take the current WgpuResources.
    /// This is intended to be called from a worker thread right before synchronizing with the main thread.   
    pub fn finish(mut self) -> (Option<wgpu::CommandBuffer>, T) {
        (
            self.command_encoder.take().map(|encoder| encoder.finish()),
            self.render_resources,
        )
    }

    /// Consume this context, finalize the current CommandEncoder (if it exists), and take the current WgpuResources.
    /// This is intended to be called from a worker thread right before synchronizing with the main thread.   
    pub fn finish_encoder(&mut self) -> Option<wgpu::CommandBuffer> {
        self.command_encoder.take().map(|encoder| encoder.finish())
    }
}

impl<T> RenderContext for WgpuRenderContext<T>
where
    T: RenderResourceContext + WgpuRenderResourceContextTrait,
{
    fn create_texture_with_data(
        &mut self,
        texture_descriptor: &TextureDescriptor,
        bytes: &[u8],
    ) -> RenderResource {
        self.render_resources.create_texture_with_data(
            self.command_encoder.get_or_create(&self.device),
            texture_descriptor,
            bytes,
        )
    }
    fn copy_buffer_to_buffer(
        &mut self,
        source_buffer: RenderResource,
        source_offset: u64,
        destination_buffer: RenderResource,
        destination_offset: u64,
        size: u64,
    ) {
        let command_encoder = self.command_encoder.get_or_create(&self.device);
        let source = self.render_resources.get_buffer(source_buffer).unwrap();
        let destination = self
            .render_resources
            .get_buffer(destination_buffer)
            .unwrap();
        command_encoder.copy_buffer_to_buffer(
            source,
            source_offset,
            destination,
            destination_offset,
            size,
        );
    }
    fn resources(&self) -> &dyn RenderResourceContext {
        &self.render_resources
    }
    fn resources_mut(&mut self) -> &mut dyn RenderResourceContext {
        &mut self.render_resources
    }
    fn create_bind_group(
        &mut self,
        bind_group_descriptor: &BindGroupDescriptor,
        render_resource_assignments: &RenderResourceAssignments,
    ) -> Option<RenderResourceSetId> {
        if let Some((render_resource_set_id, _indices)) =
            render_resource_assignments.get_render_resource_set_id(bind_group_descriptor.id)
        {
            if let None = self
                .render_resources
                .get_bind_group(bind_group_descriptor.id, *render_resource_set_id)
            {
                log::trace!(
                    "start creating bind group for RenderResourceSet {:?}",
                    render_resource_set_id
                );
                let wgpu_bind_group = {
                    let bindings = bind_group_descriptor
                        .bindings
                        .iter()
                        .map(|binding| {
                            if let Some(resource) = render_resource_assignments.get(&binding.name) {
                                let resource_info =
                                    self.resources().get_resource_info(resource).unwrap();
                                log::trace!(
                                    "found binding {} ({}) resource: {:?} {:?}",
                                    binding.index,
                                    binding.name,
                                    resource,
                                    resource_info
                                );
                                wgpu::Binding {
                                    binding: binding.index,
                                    resource: match &binding.bind_type {
                                        BindType::SampledTexture { .. } => {
                                            if let ResourceInfo::Texture = resource_info {
                                                let texture = self
                                                    .render_resources
                                                    .get_texture(resource)
                                                    .unwrap();
                                                wgpu::BindingResource::TextureView(texture)
                                            } else {
                                                panic!("expected a Texture resource");
                                            }
                                        }
                                        BindType::Sampler { .. } => {
                                            if let ResourceInfo::Sampler = resource_info {
                                                let sampler = self
                                                    .render_resources
                                                    .get_sampler(resource)
                                                    .unwrap();
                                                wgpu::BindingResource::Sampler(sampler)
                                            } else {
                                                panic!("expected a Sampler resource");
                                            }
                                        }
                                        BindType::Uniform { .. } => {
                                            if let ResourceInfo::Buffer(buffer_info) = resource_info
                                            {
                                                let buffer = self
                                                    .render_resources
                                                    .get_buffer(resource)
                                                    .unwrap();
                                                wgpu::BindingResource::Buffer {
                                                    buffer,
                                                    range: 0..buffer_info.size as u64,
                                                }
                                            } else {
                                                panic!("expected a Buffer resource");
                                            }
                                        }
                                        _ => panic!("unsupported bind type"),
                                    },
                                }
                            } else {
                                panic!(
                        "No resource assigned to uniform \"{}\" for RenderResourceAssignments {:?}",
                        binding.name,
                        render_resource_assignments.id
                    );
                            }
                        })
                        .collect::<Vec<wgpu::Binding>>();
                    let bind_group_layout = self
                        .render_resources
                        .get_bind_group_layout(bind_group_descriptor.id)
                        .unwrap();
                    let wgpu_bind_group_descriptor = wgpu::BindGroupDescriptor {
                        label: None,
                        layout: bind_group_layout,
                        bindings: bindings.as_slice(),
                    };
                    self.render_resources
                        .create_bind_group(*render_resource_set_id, &wgpu_bind_group_descriptor)
                };
                self.render_resources.set_bind_group(
                    bind_group_descriptor.id,
                    *render_resource_set_id,
                    wgpu_bind_group,
                );
                return Some(*render_resource_set_id);
            }
        }

        None
    }
    fn create_render_pipeline(
        &mut self,
        pipeline_handle: Handle<PipelineDescriptor>,
        pipeline_descriptor: &mut PipelineDescriptor,
        shader_storage: &AssetStorage<Shader>,
    ) {
        if let Some(_) = self.render_resources.get_pipeline(pipeline_handle) {
            return;
        }

        let layout = pipeline_descriptor.get_layout().unwrap();
        for bind_group in layout.bind_groups.iter() {
            if let None = self.render_resources.get_bind_group_layout(bind_group.id) {
                let bind_group_layout_binding = bind_group
                    .bindings
                    .iter()
                    .map(|binding| wgpu::BindGroupLayoutEntry {
                        binding: binding.index,
                        visibility: wgpu::ShaderStage::VERTEX | wgpu::ShaderStage::FRAGMENT,
                        ty: (&binding.bind_type).wgpu_into(),
                    })
                    .collect::<Vec<wgpu::BindGroupLayoutEntry>>();
                self.render_resources.create_bind_group_layout(
                    bind_group.id,
                    &wgpu::BindGroupLayoutDescriptor {
                        bindings: bind_group_layout_binding.as_slice(),
                        label: None,
                    },
                );
            }
        }

        // setup and collect bind group layouts
        let bind_group_layouts = layout
            .bind_groups
            .iter()
            .map(|bind_group| {
                self.render_resources
                    .get_bind_group_layout(bind_group.id)
                    .unwrap()
            })
            .collect::<Vec<&wgpu::BindGroupLayout>>();

        let pipeline_layout = self
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                bind_group_layouts: bind_group_layouts.as_slice(),
            });

        let owned_vertex_buffer_descriptors = layout
            .vertex_buffer_descriptors
            .iter()
            .map(|v| v.wgpu_into())
            .collect::<Vec<OwnedWgpuVertexBufferDescriptor>>();

        let color_states = pipeline_descriptor
            .color_states
            .iter()
            .map(|c| c.wgpu_into())
            .collect::<Vec<wgpu::ColorStateDescriptor>>();

        if let None = self
            .render_resources
            .get_shader_module(pipeline_descriptor.shader_stages.vertex)
        {
            self.render_resources
                .create_shader_module(pipeline_descriptor.shader_stages.vertex, shader_storage);
        }

        if let Some(fragment_handle) = pipeline_descriptor.shader_stages.fragment {
            if let None = self.render_resources.get_shader_module(fragment_handle) {
                self.render_resources
                    .create_shader_module(fragment_handle, shader_storage);
            }
        };
        let wgpu_pipeline = {
            let vertex_shader_module = self
                .render_resources
                .get_shader_module(pipeline_descriptor.shader_stages.vertex)
                .unwrap();

            let fragment_shader_module = match pipeline_descriptor.shader_stages.fragment {
                Some(fragment_handle) => Some(
                    self.render_resources
                        .get_shader_module(fragment_handle)
                        .unwrap(),
                ),
                None => None,
            };

            let render_pipeline_descriptor = wgpu::RenderPipelineDescriptor {
                layout: &pipeline_layout,
                vertex_stage: wgpu::ProgrammableStageDescriptor {
                    module: &vertex_shader_module,
                    entry_point: "main",
                },
                fragment_stage: match pipeline_descriptor.shader_stages.fragment {
                    Some(_) => Some(wgpu::ProgrammableStageDescriptor {
                        entry_point: "main",
                        module: fragment_shader_module.as_ref().unwrap(),
                    }),
                    None => None,
                },
                rasterization_state: pipeline_descriptor
                    .rasterization_state
                    .as_ref()
                    .map(|r| r.wgpu_into()),
                primitive_topology: pipeline_descriptor.primitive_topology.wgpu_into(),
                color_states: &color_states,
                depth_stencil_state: pipeline_descriptor
                    .depth_stencil_state
                    .as_ref()
                    .map(|d| d.wgpu_into()),
                vertex_state: wgpu::VertexStateDescriptor {
                    index_format: pipeline_descriptor.index_format.wgpu_into(),
                    vertex_buffers: &owned_vertex_buffer_descriptors
                        .iter()
                        .map(|v| v.into())
                        .collect::<Vec<wgpu::VertexBufferDescriptor>>(),
                },
                sample_count: pipeline_descriptor.sample_count,
                sample_mask: pipeline_descriptor.sample_mask,
                alpha_to_coverage_enabled: pipeline_descriptor.alpha_to_coverage_enabled,
            };

            self.render_resources
                .create_render_pipeline(&render_pipeline_descriptor)
        };
        self.render_resources
            .set_render_pipeline(pipeline_handle, wgpu_pipeline);
    }
    fn begin_pass(
        &mut self,
        pass_descriptor: &PassDescriptor,
        render_resource_assignments: &RenderResourceAssignments,
        run_pass: &mut dyn Fn(&mut dyn RenderPass),
    ) {
        if !self.command_encoder.is_some() {
            self.command_encoder.create(&self.device);
        }

        let mut encoder = self.command_encoder.take().unwrap();
        {
            let render_pass = create_render_pass(
                self,
                pass_descriptor,
                render_resource_assignments,
                &mut encoder,
            );
            let mut wgpu_render_pass = WgpuRenderPass {
                render_context: self,
                render_pass,
                bound_bind_groups: HashMap::default(),
            };

            run_pass(&mut wgpu_render_pass);
        }

        self.command_encoder.set(encoder);
    }
}

pub fn create_render_pass<'a, 'b, T>(
    render_context: &'a WgpuRenderContext<T>,
    pass_descriptor: &PassDescriptor,
    global_render_resource_assignments: &'b RenderResourceAssignments,
    encoder: &'a mut wgpu::CommandEncoder,
) -> wgpu::RenderPass<'a>
where
    T: WgpuRenderResourceContextTrait + RenderResourceContext,
{
    encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        color_attachments: &pass_descriptor
            .color_attachments
            .iter()
            .map(|c| {
                create_wgpu_color_attachment_descriptor(
                    render_context,
                    global_render_resource_assignments,
                    c,
                )
            })
            .collect::<Vec<wgpu::RenderPassColorAttachmentDescriptor>>(),
        depth_stencil_attachment: pass_descriptor.depth_stencil_attachment.as_ref().map(|d| {
            create_wgpu_depth_stencil_attachment_descriptor(
                render_context,
                global_render_resource_assignments,
                d,
            )
        }),
    })
}

fn get_texture_view<'a, T>(
    render_context: &'a WgpuRenderContext<T>,
    global_render_resource_assignments: &RenderResourceAssignments,
    name: &str,
) -> &'a wgpu::TextureView
where
    T: WgpuRenderResourceContextTrait + RenderResourceContext,
{
    match name {
        resource_name::texture::SWAP_CHAIN => {
            if let Some(primary_swap_chain) = render_context
                .render_resources
                .get_swap_chain_output(render_context.primary_window.as_ref().unwrap())
                .map(|output| &output.view)
            {
                primary_swap_chain
            } else {
                panic!("No primary swap chain found for color attachment");
            }
        }
        _ => match global_render_resource_assignments.get(name) {
            Some(resource) => render_context
                .render_resources
                .get_texture(resource)
                .unwrap(),
            None => {
                // if let Some(swap_chain_output) = swap_chain_outputs.get(name) {
                //     &swap_chain_output.view
                // } else {
                panic!("Color attachment {} does not exist", name);
                // }
            }
        },
    }
}

fn create_wgpu_color_attachment_descriptor<'a, T>(
    render_context: &'a WgpuRenderContext<T>,
    global_render_resource_assignments: &RenderResourceAssignments,
    color_attachment_descriptor: &RenderPassColorAttachmentDescriptor,
) -> wgpu::RenderPassColorAttachmentDescriptor<'a>
where
    T: WgpuRenderResourceContextTrait + RenderResourceContext,
{
    let attachment = get_texture_view(
        render_context,
        global_render_resource_assignments,
        color_attachment_descriptor.attachment.as_str(),
    );

    let resolve_target = color_attachment_descriptor
        .resolve_target
        .as_ref()
        .map(|target| {
            get_texture_view(
                render_context,
                global_render_resource_assignments,
                target.as_str(),
            )
        });

    wgpu::RenderPassColorAttachmentDescriptor {
        store_op: color_attachment_descriptor.store_op.wgpu_into(),
        load_op: color_attachment_descriptor.load_op.wgpu_into(),
        clear_color: color_attachment_descriptor.clear_color.wgpu_into(),
        attachment,
        resolve_target,
    }
}

fn create_wgpu_depth_stencil_attachment_descriptor<'a, T>(
    render_context: &'a WgpuRenderContext<T>,
    global_render_resource_assignments: &RenderResourceAssignments,
    depth_stencil_attachment_descriptor: &RenderPassDepthStencilAttachmentDescriptor,
) -> wgpu::RenderPassDepthStencilAttachmentDescriptor<'a>
where
    T: WgpuRenderResourceContextTrait + RenderResourceContext,
{
    let attachment = get_texture_view(
        render_context,
        global_render_resource_assignments,
        depth_stencil_attachment_descriptor.attachment.as_str(),
    );

    wgpu::RenderPassDepthStencilAttachmentDescriptor {
        attachment,
        clear_depth: depth_stencil_attachment_descriptor.clear_depth,
        clear_stencil: depth_stencil_attachment_descriptor.clear_stencil,
        depth_load_op: depth_stencil_attachment_descriptor
            .depth_load_op
            .wgpu_into(),
        depth_store_op: depth_stencil_attachment_descriptor
            .depth_store_op
            .wgpu_into(),
        stencil_load_op: depth_stencil_attachment_descriptor
            .stencil_load_op
            .wgpu_into(),
        stencil_store_op: depth_stencil_attachment_descriptor
            .stencil_store_op
            .wgpu_into(),
    }
}