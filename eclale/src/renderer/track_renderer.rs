use anyhow::{anyhow, Result};

use bytemuck::{Pod, Zeroable};
use nalgebra::{
    Isometry3, Matrix4, Orthographic3, Perspective3, Point3, Vector2, Vector3, Vector4,
};
use winit::raw_window_handle::{RawDisplayHandle, RawWindowHandle};

use eclale_graphics::{
    geometry::{
        line::{cubic_bezier_curve_point_at_pos, Curve},
        plane::Plane,
        polyhedron::Polyhedron,
        torus::TorusBuilder,
        Mesh,
    },
    renderer::{
        render_description::{
            InstancedDrawData, MOSVDrawData, RenderDescription, RenderPipelineDescription,
            RenderingType,
        },
        Renderer,
    },
    vulkan::{
        shader::{ShaderModuleDescriptor, ShaderStage},
        vk,
    },
};

use super::track_description::{NoteInstance, PlatformInstance, TrackDescription};

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Default)]
struct SceneUniformGpuData {
    view_projection: Matrix4<f32>,
    runner_transform: Matrix4<f32>,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Default)]
struct ObjectInstanceGpuData {
    transform: Matrix4<f32>,
    base_color: Vector4<f32>,
    apply_runner_transform: u32,

    _pad0: u32,
    _pad1: u32,
    _pad2: u32,
}

impl ObjectInstanceGpuData {
    fn from_note_instance(instance: &NoteInstance) -> Self {
        Self {
            transform: Matrix4::new_translation(&Vector3::new(
                instance.x_position,
                0.0,
                instance.z_position,
            )),
            base_color: instance.base_color,
            apply_runner_transform: if instance.apply_runner_transform {
                1
            } else {
                0
            },

            ..Default::default()
        }
    }

    fn from_note_instances(instances: &[NoteInstance]) -> Vec<Self> {
        instances.iter().map(Self::from_note_instance).collect()
    }

    fn from_platform_instance(instance: &PlatformInstance) -> Self {
        assert!(instance.z_start_position == 0.0);
        Self {
            transform: Matrix4::new_translation(&Vector3::new(0.0, 0.0, instance.z_start_position)),
            base_color: instance.base_color,
            apply_runner_transform: 1,

            ..Default::default()
        }
    }

    fn from_platform_instances(instances: &[PlatformInstance]) -> Vec<Self> {
        instances.iter().map(Self::from_platform_instance).collect()
    }

    // fn from_hold_note_instance(instance: &HoldNoteInstance) -> Self {
    //     Self {
    //         transform: Matrix4::new_translation(&Vector3::new(0.0, 0.0, instance.z_start_position)),
    //         base_color: instance.base_color,
    //         apply_runner_transform: 1,
    //
    //         ..Default::default()
    //     }
    // }

    fn create_bytes(instances: &[Self]) -> Vec<u8> {
        instances
            .iter()
            .flat_map(|o| bytemuck::bytes_of(o).to_vec())
            .collect()
    }
}

pub(crate) const HIT_Z_LENGTH: f32 = 0.1;
pub(crate) const HIT_X_LENGTH: f32 = 0.2;

struct RenderMeshes {
    hit: Mesh,
    contact: Mesh,
    evade: Mesh,
    flick: Mesh,
}

impl RenderMeshes {
    fn new() -> Self {
        let hit = Mesh::from(Polyhedron::cuboid(HIT_X_LENGTH, 0.1, HIT_Z_LENGTH));
        let contact = {
            let torus_builder = TorusBuilder::new(0.05, 0.02, 30, 30);
            torus_builder.build_mesh().transform(&Matrix4::new_rotation(
                Vector3::x() * std::f32::consts::FRAC_PI_2,
            ))
        };
        let evade = Polyhedron::icosahedron(0.12).into();
        let flick = Polyhedron::cuboid(0.9, 0.05, HIT_Z_LENGTH).into();

        Self {
            hit,
            contact,
            evade,
            flick,
        }
    }

    fn hit(&self) -> Mesh {
        self.hit.clone()
    }

    fn contact(&self) -> Mesh {
        self.contact.clone()
    }

    fn evade(&self) -> Mesh {
        self.evade.clone()
    }

    fn flick(&self) -> Mesh {
        self.flick.clone()
    }
}

struct RenderPipelines;

impl RenderPipelines {
    fn instanced() -> RenderPipelineDescription {
        RenderPipelineDescription {
            rendering_type: RenderingType::Instanced,
            shader_modules: vec![
                ShaderModuleDescriptor {
                    source_file_name: String::from("shaders/object_instanced.vs.glsl"),
                    shader_stage: ShaderStage::Vertex,
                },
                ShaderModuleDescriptor {
                    source_file_name: String::from("shaders/object_instanced.fs.glsl"),
                    shader_stage: ShaderStage::Fragment,
                },
            ],
        }
    }

    // Multiple objects and one vertex stream for all objects' vertex data.
    fn mosv_planes_smooth() -> RenderPipelineDescription {
        RenderPipelineDescription {
            rendering_type: RenderingType::MultipleObjectsSingleVertexData,
            shader_modules: vec![
                ShaderModuleDescriptor {
                    source_file_name: String::from("shaders/object_vertices_smooth_1.vs.glsl"),
                    shader_stage: ShaderStage::Vertex,
                },
                ShaderModuleDescriptor {
                    source_file_name: String::from("shaders/object_vertices_smooth_1.fs.glsl"),
                    shader_stage: ShaderStage::Fragment,
                },
            ],
        }
    }

    fn mosv_lines_smooth() -> RenderPipelineDescription {
        RenderPipelineDescription {
            rendering_type: RenderingType::MultipleObjectsSingleVertexData,
            shader_modules: vec![
                ShaderModuleDescriptor {
                    source_file_name: String::from("shaders/object_vertices_smooth_2.vs.glsl"),
                    shader_stage: ShaderStage::Vertex,
                },
                ShaderModuleDescriptor {
                    source_file_name: String::from("shaders/object_vertices_smooth_2.fs.glsl"),
                    shader_stage: ShaderStage::Fragment,
                },
            ],
        }
    }
}

struct RenderDescriptionCreator {
    description: TrackDescription,

    meshes: RenderMeshes,

    pipelines: Vec<RenderPipelineDescription>,
    instanced_draw_data: Vec<InstancedDrawData>,
    mosv_draw_data: Vec<MOSVDrawData>,
}

impl RenderDescriptionCreator {
    fn new(description: TrackDescription) -> Self {
        Self {
            description,
            meshes: RenderMeshes::new(),
            pipelines: Vec::new(),
            instanced_draw_data: Vec::new(),
            mosv_draw_data: Vec::new(),
        }
    }

    fn add_pipeline(&mut self, pipeline: RenderPipelineDescription) -> usize {
        self.pipelines.push(pipeline);
        println!("Returning pipeline index {}", self.pipelines.len() - 1);
        self.pipelines.len() - 1
    }

    fn add_instanced_draw_data(&mut self, draw_data: InstancedDrawData) {
        self.instanced_draw_data.push(draw_data);
    }

    fn add_objects_instanced_draw_data(
        &mut self,
        instances: &[ObjectInstanceGpuData],
        mesh: Mesh,
        pipeline_index: usize,
    ) {
        self.add_instanced_draw_data(InstancedDrawData {
            instance_count: instances.len(),
            instance_data: ObjectInstanceGpuData::create_bytes(instances),
            vertices: mesh.vertices,
            indices: mesh.indices,
            pipeline_index,
        })
    }

    fn add_mosv_draw_data(
        &mut self,
        pipeline_index: usize,
        mesh: Mesh,
        objects: &[ObjectInstanceGpuData],
        objects_indices: &[usize],
    ) {
        self.mosv_draw_data.push(MOSVDrawData {
            objects_count: objects.len(),
            objects_data: ObjectInstanceGpuData::create_bytes(objects),
            objects_indices: bytemuck::cast_slice(
                &objects_indices
                    .iter()
                    .map(|i| *i as u32)
                    .collect::<Vec<_>>(),
            )
            .to_vec(),
            vertices: mesh.vertices,
            indices: mesh.indices,
            pipeline_index,
        });
    }

    fn create(mut self) -> RenderDescription {
        let pipeline_index_instanced = self.add_pipeline(RenderPipelines::instanced());
        let pipeline_index_mosv_planes = self.add_pipeline(RenderPipelines::mosv_planes_smooth());
        let pipeline_index_mosv_lines = self.add_pipeline(RenderPipelines::mosv_lines_smooth());

        self.add_objects_instanced_draw_data(
            &ObjectInstanceGpuData::from_note_instances(&self.description.notes_hit),
            self.meshes.hit(),
            pipeline_index_instanced,
        );
        self.add_objects_instanced_draw_data(
            &ObjectInstanceGpuData::from_note_instances(&self.description.notes_contact),
            self.meshes.contact(),
            pipeline_index_instanced,
        );
        self.add_objects_instanced_draw_data(
            &ObjectInstanceGpuData::from_note_instances(&self.description.notes_flick),
            self.meshes.flick(),
            pipeline_index_instanced,
        );

        self.add_objects_instanced_draw_data(
            &ObjectInstanceGpuData::from_platform_instances(&self.description.platform_instances),
            self.description.platform_mesh.clone(),
            pipeline_index_instanced,
        );

        self.add_mosv_draw_data(
            pipeline_index_mosv_planes,
            self.description.hold_notes.mesh.clone(),
            &ObjectInstanceGpuData::from_platform_instances(&self.description.hold_notes.objects),
            &self.description.hold_notes.objects_indices.clone(),
        );

        self.add_mosv_draw_data(
            pipeline_index_mosv_lines,
            self.description.lanes.mesh.clone(),
            &ObjectInstanceGpuData::from_platform_instances(&self.description.lanes.objects),
            &self.description.lanes.objects_indices.clone(),
        );

        RenderDescription {
            scene_uniform_data_size: std::mem::size_of::<SceneUniformGpuData>() as _,
            pipelines: self.pipelines,
            instanced_draw_data: self.instanced_draw_data,
            mosv_draw_data: self.mosv_draw_data,
        }
    }
}

pub(crate) struct TrackRenderer {
    renderer: Renderer,
    track_description: TrackDescription,
    render_description: RenderDescription,
    scene_uniform: SceneUniformGpuData,
}

impl TrackRenderer {
    pub(crate) fn new(
        window_handle: RawWindowHandle,
        display_handle: RawDisplayHandle,
        track_description: TrackDescription,
    ) -> Result<Self> {
        let render_description = RenderDescriptionCreator::new(track_description.clone()).create();
        let renderer = Renderer::new(window_handle, display_handle, render_description.clone())?;

        Ok(Self {
            renderer,
            track_description,
            render_description,
            scene_uniform: SceneUniformGpuData::default(),
        })
    }

    pub(crate) fn render(&mut self) -> Result<()> {
        self.renderer
            .update_scene_uniform_data(bytemuck::bytes_of(&self.scene_uniform));
        self.renderer.render()?;

        Ok(())
    }

    pub(crate) fn update_view_projection(&mut self, view_projection: Matrix4<f32>) {
        self.scene_uniform.view_projection = view_projection;
    }

    pub(crate) fn update_runner_position(&mut self, runner_position: f32) {
        self.scene_uniform.runner_transform =
            Matrix4::new_translation(&Vector3::new(0.0, 0.0, -runner_position));
    }

    pub(crate) fn swapchain_extent(&self) -> Vector2<u32> {
        self.renderer.swapchain_extent()
    }
}