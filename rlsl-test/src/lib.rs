extern crate gfx_backend_vulkan as back;
#[macro_use]
extern crate quickcheck;
//extern crate glsl_to_spirv;
extern crate gfx_hal as hal;
extern crate issues;

use quickcheck::TestResult;
use hal::{buffer, command, memory, pool, pso, queue};
use hal::{Backend, Compute, DescriptorPool, Device, Instance, PhysicalDevice, QueueFamily};
use std::mem::size_of;
use std::fs;
use std::path::Path;

pub fn compute<T, P, F>(name: &str, numbers: Vec<T>, path: P, f: F) -> TestResult
where
    F: Fn(u32, T) -> T,
    P: AsRef<Path>,
    T: Copy + Clone + PartialEq,
{
    if numbers.is_empty() {
        return TestResult::discard();
    }

    let mut numbers_cpu = numbers.clone();
    let stride = size_of::<T>() as u64;

    let instance = back::Instance::create("gfx-rs compute", 1);

    let mut adapter = instance
        .enumerate_adapters()
        .into_iter()
        .find(|a| {
            a.queue_families
                .iter()
                .any(|family| family.supports_compute())
        })
        .expect("Failed to find a GPU with compute support!");

    let memory_properties = adapter.physical_device.memory_properties();
    let (mut device, mut queue_group) = adapter.open_with::<_, Compute>(1, |_family| true).unwrap();

    let mut spirv_file = fs::File::open(path.as_ref()).expect("file");
    use std::io::Read;
    let mut data = Vec::new();
    spirv_file.read_to_end(&mut data).expect("read");

    let shader = device.create_shader_module(&data).unwrap();
    println!("Shader");

    let (pipeline_layout, pipeline, set_layout, mut desc_pool) = {
        let set_layout = device.create_descriptor_set_layout(
            &[pso::DescriptorSetLayoutBinding {
                binding: 0,
                ty: pso::DescriptorType::StorageBuffer,
                count: 1,
                stage_flags: pso::ShaderStageFlags::COMPUTE,
                immutable_samplers: false,
            }],
            &[],
        );
        println!("Descr");

        let pipeline_layout = device.create_pipeline_layout(Some(&set_layout), &[]);
        let entry_point = pso::EntryPoint {
            entry: name,
            module: &shader,
            specialization: &[],
        };
        println!("Layout");
        let pipeline = device
            .create_compute_pipeline(&pso::ComputePipelineDesc::new(
                entry_point,
                &pipeline_layout,
            ))
            .expect("Error creating compute pipeline!");

        println!("Pipe");
        let desc_pool = device.create_descriptor_pool(
            1,
            &[pso::DescriptorRangeDesc {
                ty: pso::DescriptorType::StorageBuffer,
                count: 1,
            }],
        );
        println!("Pool");
        (pipeline_layout, pipeline, set_layout, desc_pool)
    };

    let staging_buffer = create_buffer::<back::Backend>(
        &mut device,
        &memory_properties.memory_types,
        memory::Properties::CPU_VISIBLE | memory::Properties::COHERENT,
        buffer::Usage::TRANSFER_SRC | buffer::Usage::TRANSFER_DST,
        stride,
        numbers.len() as u64,
    );

    {
        let mut writer = device
            .acquire_mapping_writer::<T>(&staging_buffer.memory, 0..stride * numbers.len() as u64)
            .unwrap();
        writer.copy_from_slice(&numbers);
        device.release_mapping_writer(writer);
    }

    let device_buffer = create_buffer::<back::Backend>(
        &mut device,
        &memory_properties.memory_types,
        memory::Properties::DEVICE_LOCAL,
        buffer::Usage::TRANSFER_SRC | buffer::Usage::TRANSFER_DST | buffer::Usage::STORAGE,
        stride,
        numbers.len() as u64,
    );

    let desc_set = desc_pool.allocate_set(&set_layout).unwrap();
    device.write_descriptor_sets(Some(pso::DescriptorSetWrite {
        set: &desc_set,
        binding: 0,
        array_offset: 0,
        descriptors: Some(pso::Descriptor::Buffer(&device_buffer.buffer, None..None)),
    }));

    let mut command_pool =
        device.create_command_pool_typed(&queue_group, pool::CommandPoolCreateFlags::empty(), 16);
    let fence = device.create_fence(false);
    let submission = queue::Submission::new().submit(Some({
        let mut command_buffer = command_pool.acquire_command_buffer(false);
        command_buffer.copy_buffer(
            &staging_buffer.buffer,
            &device_buffer.buffer,
            &[command::BufferCopy {
                src: 0,
                dst: 0,
                size: stride * numbers.len() as u64,
            }],
        );
        command_buffer.pipeline_barrier(
            pso::PipelineStage::TRANSFER..pso::PipelineStage::COMPUTE_SHADER,
            memory::Dependencies::empty(),
            Some(memory::Barrier::Buffer {
                states: buffer::Access::TRANSFER_WRITE
                    ..buffer::Access::SHADER_READ | buffer::Access::SHADER_WRITE,
                target: &device_buffer.buffer,
            }),
        );
        command_buffer.bind_compute_pipeline(&pipeline);
        command_buffer.bind_compute_descriptor_sets(&pipeline_layout, 0, &[desc_set], &[]);
        command_buffer.dispatch([numbers.len() as u32, 1, 1]);
        command_buffer.pipeline_barrier(
            pso::PipelineStage::COMPUTE_SHADER..pso::PipelineStage::TRANSFER,
            memory::Dependencies::empty(),
            Some(memory::Barrier::Buffer {
                states: buffer::Access::SHADER_READ | buffer::Access::SHADER_WRITE
                    ..buffer::Access::TRANSFER_READ,
                target: &device_buffer.buffer,
            }),
        );
        command_buffer.copy_buffer(
            &device_buffer.buffer,
            &staging_buffer.buffer,
            &[command::BufferCopy {
                src: 0,
                dst: 0,
                size: stride * numbers.len() as u64,
            }],
        );
        command_buffer.finish()
    }));
    queue_group.queues[0].submit(submission, Some(&fence));
    device.wait_for_fence(&fence, !0);

    let eq = {
        let reader = device
            .acquire_mapping_reader::<T>(&staging_buffer.memory, 0..stride * numbers.len() as u64)
            .unwrap();
        let numbers_gpu = reader.into_iter().map(|n| *n).collect::<Vec<T>>();
        numbers_cpu.iter_mut().enumerate().for_each(|(index, val)| {
            let result = f(index as u32, *val);
            *val = result;
        });
        device.release_mapping_reader(reader);
        numbers_gpu == numbers_cpu
    };

    device.destroy_command_pool(command_pool.into_raw());
    device.destroy_descriptor_pool(desc_pool);
    device.destroy_descriptor_set_layout(set_layout);
    device.destroy_shader_module(shader);
    device.destroy_buffer(device_buffer.buffer);
    device.destroy_buffer(staging_buffer.buffer);
    device.destroy_fence(fence);
    device.destroy_pipeline_layout(pipeline_layout);
    device.free_memory(device_buffer.memory);
    device.free_memory(staging_buffer.memory);
    device.destroy_compute_pipeline(pipeline);

    TestResult::from_bool(eq)
}

pub struct Buffer<B: Backend> {
    pub memory: B::Memory,
    pub buffer: B::Buffer,
    pub requirements: memory::Requirements
}

fn create_buffer<B: Backend>(
    device: &mut B::Device,
    memory_types: &[hal::MemoryType],
    properties: memory::Properties,
    usage: buffer::Usage,
    stride: u64,
    len: u64,
) -> Buffer<B> {
    let buffer = device.create_buffer(stride * len, usage).unwrap();
    let requirements = device.get_buffer_requirements(&buffer);

    let ty = memory_types
        .into_iter()
        .enumerate()
        .position(|(id, memory_type)| {
            requirements.type_mask & (1 << id) != 0 && memory_type.properties.contains(properties)
        })
        .unwrap()
        .into();

    let memory = device.allocate_memory(ty, requirements.size).unwrap();
    let buffer = device.bind_buffer_memory(&memory, 0, buffer).unwrap();

    Buffer {
        memory, buffer, requirements
    }
}

#[cfg(test)]
mod tests {
    use compute;
    use issues;
    use quickcheck::TestResult;
    quickcheck! {
        fn compute_u32_add(input: Vec<f32>) -> TestResult {
            compute("compute", input, "../.shaders/u32-add.spv", issues::u32_add)
        }
        fn compute_square(input: Vec<f32>) -> TestResult {
            compute("compute", input, "../.shaders/square.spv", issues::square)
        }

        fn compute_single_branch(input: Vec<f32>) -> TestResult {
            compute("compute", input, "../.shaders/single-branch.spv", issues::single_branch)
        }

        fn compute_single_branch_glsl(input: Vec<f32>) -> TestResult {
            compute("main", input, "../issues/.shaders-glsl/single-branch.spv", issues::single_branch)
        }
    }

}
