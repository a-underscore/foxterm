use crate::{
    item::{
        mesh::{Mesh, Vertex},
        texture::Texture,
        Item,
    },
    loaded_font::LoadedFont,
    shaders::{fragment, vertex, Shaders},
    terminal::{drawable::RenderItem, Performer, Terminal},
    APP_NAME,
};
use cgmath::{Matrix4, Vector2};
use std::sync::Arc;
use vulkano::{
    buffer::{cpu_pool::CpuBufferPool, BufferUsage, TypedBufferAccess},
    command_buffer::{
        pool::standard::StandardCommandPoolBuilder, AutoCommandBufferBuilder, CommandBufferUsage,
        PrimaryAutoCommandBuffer, SubpassContents,
    },
    descriptor_set::{PersistentDescriptorSet, WriteDescriptorSet},
    device::{
        physical::{PhysicalDevice, PhysicalDeviceType},
        Device, DeviceCreateInfo, DeviceExtensions, QueueCreateInfo,
    },
    format::Format,
    image::{
        attachment::AttachmentImage, view::ImageView, ImageAccess, ImageUsage, SwapchainImage,
    },
    instance::{Instance, InstanceCreateInfo},
    pipeline::{
        graphics::{
            color_blend::ColorBlendState,
            depth_stencil::DepthStencilState,
            input_assembly::{InputAssemblyState, PrimitiveTopology},
            vertex_input::BuffersDefinition,
            viewport::{Viewport, ViewportState},
            GraphicsPipeline,
        },
        Pipeline, PipelineBindPoint,
    },
    render_pass::{Framebuffer, FramebufferCreateInfo, RenderPass, Subpass},
    swapchain::{self, AcquireError, Swapchain, SwapchainCreateInfo, SwapchainCreationError},
    sync::{self, FlushError, GpuFuture},
};
use vulkano_win::VkSurfaceBuild;
use winit::{
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoop},
    window::{Window, WindowBuilder},
};
use winit_input_helper::WinitInputHelper;

pub struct Renderer;

impl Renderer {
    pub fn init(terminal: Terminal) -> anyhow::Result<()> {
        let proj = cgmath::ortho::<f32>(-1.0, 1.0, -1.0, 1.0, -1.0, 1.0);
        let required_extensions = vulkano_win::required_extensions();
        let instance = Instance::new(InstanceCreateInfo {
            enabled_extensions: required_extensions,
            ..Default::default()
        })?;
        let event_loop = EventLoop::new();
        let surface = WindowBuilder::new()
            .with_title(APP_NAME)
            .build_vk_surface(&event_loop, instance.clone())?;
        let device_extensions = DeviceExtensions {
            khr_swapchain: true,
            ..DeviceExtensions::none()
        };
        let (physical_device, queue_family) = {
            let mut devices = PhysicalDevice::enumerate(&instance);

            match terminal.config.device_index {
                Some(physical_index) => {
                    let device = devices.nth(physical_index).unwrap();

                    device
                        .queue_families()
                        .find(|&q| {
                            q.supports_graphics() && q.supports_surface(&surface).unwrap_or(false)
                        })
                        .map(|q| (device, q))
                        .unwrap()
                }
                None => devices
                    .filter(|&p| p.supported_extensions().is_superset_of(&device_extensions))
                    .filter_map(|p| {
                        p.queue_families()
                            .find(|&q| {
                                q.supports_graphics()
                                    && q.supports_surface(&surface).unwrap_or(false)
                            })
                            .map(|q| (p, q))
                    })
                    .min_by_key(|(p, _)| match p.properties().device_type {
                        PhysicalDeviceType::DiscreteGpu => 0,
                        PhysicalDeviceType::IntegratedGpu => 1,
                        PhysicalDeviceType::VirtualGpu => 2,
                        PhysicalDeviceType::Cpu => 3,
                        PhysicalDeviceType::Other => 4,
                    })
                    .unwrap(),
            }
        };
        let (device, mut queues) = Device::new(
            physical_device,
            DeviceCreateInfo {
                enabled_extensions: physical_device
                    .required_extensions()
                    .union(&device_extensions),
                queue_create_infos: vec![QueueCreateInfo::family(queue_family)],
                ..Default::default()
            },
        )?;
        let shaders = Arc::new(Shaders::new(device.clone())?);
        let queue = queues.next().unwrap();
        let (mut swapchain, images) = {
            let surface_capabilities =
                physical_device.surface_capabilities(&surface, Default::default())?;
            let image_format =
                Some(physical_device.surface_formats(&surface, Default::default())?[0].0);

            Swapchain::new(
                device.clone(),
                surface.clone(),
                SwapchainCreateInfo {
                    min_image_count: surface_capabilities.min_image_count,
                    image_format,
                    image_extent: surface.window().inner_size().into(),
                    image_usage: ImageUsage::color_attachment(),
                    composite_alpha: surface_capabilities
                        .supported_composite_alpha
                        .iter()
                        .next()
                        .unwrap(),
                    ..Default::default()
                },
            )?
        };
        let render_pass = vulkano::single_pass_renderpass!(device.clone(),
            attachments: {
                color: {
                    load: Clear,
                    store: DontCare,
                    format: swapchain.image_format(),
                    samples: 1,
                },
                depth: {
                    load: Clear,
                    store: DontCare,
                    format: Format::D16_UNORM,
                    samples: 1,
                }
            },
            pass:
            {
                color: [color],
                depth_stencil: {depth}
            }
        )?;
        let (mut pipeline, mut framebuffers) = Self::window_size_dependent_setup(
            render_pass.clone(),
            device.clone(),
            shaders.clone(),
            &images,
        )?;
        let uniform_buffer =
            CpuBufferPool::<vertex::ty::Data>::new(device.clone(), BufferUsage::uniform_buffer());
        let frag_uniform_buffer =
            CpuBufferPool::<fragment::ty::Data>::new(device.clone(), BufferUsage::uniform_buffer());
        let font = Arc::new(LoadedFont::from_file(
            device.clone(),
            queue.clone(),
            &terminal.config,
        )?);
        let cursor = Item::new(
            Mesh::from_rect(queue.clone(), Vector2::new(font.scale / 2.0, font.scale))?,
            Texture::white(device.clone(), queue.clone())?,
        );
        let performer = terminal.spawn_reader(font);
        let write_sndr = terminal.spawn_writer();
        let mut input = WinitInputHelper::new();
        let mut recreate_swapchain = false;
        let mut previous_frame_end = Some(sync::now(device.clone()).boxed());

        event_loop.run(move |event, _, control_flow| {
            input.update(&event);

            match event {
                Event::WindowEvent {
                    event: WindowEvent::CloseRequested,
                    ..
                } => *control_flow = ControlFlow::Exit,
                Event::WindowEvent {
                    event: WindowEvent::Resized(_),
                    ..
                } => recreate_swapchain = true,
                Event::RedrawEventsCleared => {
                    terminal.update_pty(&write_sndr, &input).unwrap();

                    previous_frame_end.as_mut().unwrap().cleanup_finished();

                    if recreate_swapchain {
                        let (new_swapchain, images) =
                            match swapchain.recreate(SwapchainCreateInfo {
                                image_extent: surface.window().inner_size().into(),
                                ..swapchain.create_info()
                            }) {
                                Ok(r) => r,
                                Err(SwapchainCreationError::ImageExtentNotSupported { .. }) => {
                                    return
                                }
                                Err(e) => panic!("Failed to recreate swapchain: {:?}", e),
                            };

                        swapchain = new_swapchain;

                        let (new_pipeline, new_framebuffers) = Self::window_size_dependent_setup(
                            render_pass.clone(),
                            device.clone(),
                            shaders.clone(),
                            &images,
                        )
                        .unwrap();

                        pipeline = new_pipeline;
                        framebuffers = new_framebuffers;
                        recreate_swapchain = false;
                    }

                    let (image_num, suboptimal, acquire_future) =
                        match swapchain::acquire_next_image(swapchain.clone(), None) {
                            Ok(r) => r,

                            Err(AcquireError::OutOfDate) => {
                                recreate_swapchain = true;

                                return;
                            }

                            Err(e) => panic!("Failed to acquire next image: {:?}", e),
                        };

                    if suboptimal {
                        recreate_swapchain = true;
                    }

                    let mut builder = AutoCommandBufferBuilder::primary(
                        device.clone(),
                        queue.family(),
                        CommandBufferUsage::OneTimeSubmit,
                    )
                    .unwrap();

                    builder
                        .begin_render_pass(
                            framebuffers[image_num].clone(),
                            SubpassContents::Inline,
                            vec![terminal.config.bg_color.into(), 1_f32.into()],
                        )
                        .unwrap();

                    Self::draw_terminal(
                        &mut builder,
                        pipeline.clone(),
                        &uniform_buffer,
                        &frag_uniform_buffer,
                        &performer.read().unwrap(),
                        &cursor,
                        proj,
                        &terminal,
                    );

                    builder.end_render_pass().unwrap();

                    let command_buffer = builder.build().unwrap();
                    let future = previous_frame_end
                        .take()
                        .unwrap()
                        .join(acquire_future)
                        .then_execute(queue.clone(), command_buffer)
                        .unwrap()
                        .then_swapchain_present(queue.clone(), swapchain.clone(), image_num)
                        .then_signal_fence_and_flush();

                    match future {
                        Ok(future) => {
                            previous_frame_end = Some(future.boxed());
                        }
                        Err(FlushError::OutOfDate) => {
                            recreate_swapchain = true;
                            previous_frame_end = Some(sync::now(device.clone()).boxed());
                        }
                        Err(e) => {
                            println!("Failed to flush future: {:?}", e);

                            previous_frame_end = Some(sync::now(device.clone()).boxed());
                        }
                    }
                }
                _ => {}
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_terminal(
        builder: &mut AutoCommandBufferBuilder<
            PrimaryAutoCommandBuffer,
            StandardCommandPoolBuilder,
        >,
        pipeline: Arc<GraphicsPipeline>,
        uniform_buffer: &CpuBufferPool<vertex::ty::Data>,
        frag_uniform_buffer: &CpuBufferPool<fragment::ty::Data>,
        performer: &Performer,
        cursor: &Item,
        proj: Matrix4<f32>,
        terminal: &Terminal,
    ) {
        for drawable in &*terminal.screen.read().unwrap() {
            if let RenderItem::Chr(chr) = &drawable.render_item {
                Self::draw_item(
                    builder,
                    pipeline.clone(),
                    terminal,
                    uniform_buffer,
                    frag_uniform_buffer,
                    proj,
                    drawable.pos,
                    &chr.item,
                );
            }
        }

        Self::draw_item(
            builder,
            pipeline,
            terminal,
            uniform_buffer,
            frag_uniform_buffer,
            proj,
            performer.pos,
            cursor,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_item(
        builder: &mut AutoCommandBufferBuilder<
            PrimaryAutoCommandBuffer,
            StandardCommandPoolBuilder,
        >,
        pipeline: Arc<GraphicsPipeline>,
        terminal: &Terminal,
        uniform_buffer: &CpuBufferPool<vertex::ty::Data>,
        frag_uniform_buffer: &CpuBufferPool<fragment::ty::Data>,
        proj: Matrix4<f32>,
        pos: Vector2<f32>,
        item: &Item,
    ) {
        let uniform_buffer_subbuffer = {
            let uniform_data = vertex::ty::Data {
                proj: proj.into(),
                transform: Matrix4::from_translation(pos.extend(0.0)).into(),
            };

            Arc::new(uniform_buffer.next(uniform_data).unwrap())
        };
        let frag_uniform_buffer_subbuffer = {
            let uniform_data = fragment::ty::Data {
                color: terminal.config.font_color,
            };

            Arc::new(frag_uniform_buffer.next(uniform_data).unwrap())
        };
        let descriptor_set_layouts = pipeline.layout().set_layouts();
        let set_layout = descriptor_set_layouts.get(0).unwrap();
        let set = PersistentDescriptorSet::new(
            set_layout.clone(),
            [
                WriteDescriptorSet::buffer(0, uniform_buffer_subbuffer),
                WriteDescriptorSet::buffer(1, frag_uniform_buffer_subbuffer),
                WriteDescriptorSet::image_view_sampler(
                    2,
                    item.texture.image.clone(),
                    item.texture.sampler.clone(),
                ),
            ],
        )
        .unwrap();

        builder
            .bind_pipeline_graphics(pipeline.clone())
            .bind_descriptor_sets(
                PipelineBindPoint::Graphics,
                pipeline.layout().clone(),
                0,
                set,
            )
            .bind_vertex_buffers(0, item.mesh.vertices.clone())
            .bind_index_buffer(item.mesh.indices.clone())
            .draw_indexed(item.mesh.indices.len() as u32, 1, 0, 0, 0)
            .unwrap();
    }

    fn window_size_dependent_setup(
        render_pass: Arc<RenderPass>,
        device: Arc<Device>,
        shaders: Arc<Shaders>,
        images: &[Arc<SwapchainImage<Window>>],
    ) -> anyhow::Result<(Arc<GraphicsPipeline>, Vec<Arc<Framebuffer>>)> {
        let dimensions = images[0].dimensions().width_height();
        let depth = ImageView::new_default(AttachmentImage::transient(
            device.clone(),
            dimensions,
            Format::D16_UNORM,
        )?)?;
        let framebuffers = images
            .iter()
            .map(|image| {
                let view = ImageView::new_default(image.clone()).unwrap();

                Framebuffer::new(
                    render_pass.clone(),
                    FramebufferCreateInfo {
                        attachments: vec![view, depth.clone()],
                        ..Default::default()
                    },
                )
                .unwrap()
            })
            .collect();
        let subpass = Subpass::from(render_pass, 0).unwrap();
        let pipeline = GraphicsPipeline::start()
            .vertex_input_state(BuffersDefinition::new().vertex::<Vertex>())
            .vertex_shader(shaders.vertex.entry_point("main").unwrap(), ())
            .input_assembly_state(
                InputAssemblyState::new().topology(PrimitiveTopology::TriangleStrip),
            )
            .viewport_state(ViewportState::viewport_fixed_scissor_irrelevant([
                Viewport {
                    origin: [0.0, 0.0],
                    dimensions: [dimensions[0] as f32, dimensions[1] as f32],
                    depth_range: 0.0..1.0,
                },
            ]))
            .fragment_shader(shaders.fragment.entry_point("main").unwrap(), ())
            .depth_stencil_state(DepthStencilState::simple_depth_test())
            .color_blend_state(ColorBlendState::new(subpass.num_color_attachments()).blend_alpha())
            .render_pass(subpass)
            .build(device)?;

        Ok((pipeline, framebuffers))
    }
}
