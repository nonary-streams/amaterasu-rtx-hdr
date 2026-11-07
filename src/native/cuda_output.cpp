// CUDA resource output: the converted frame stays in CUDA device memory of
// the primary context and is ordered for the consumer by a CUDA event, so no
// GL output draw, DMA-BUF export or sync_file handoff follows the model.
#include "internal.hpp"
#include "rtx_output_ptx.h"
#include <cstring>
#include <unistd.h>

namespace rtx {
namespace {
constexpr unsigned block_size = 16;

unsigned blocks(uint32_t extent) { return (extent + block_size - 1) / block_size; }
} // namespace

void Session::load_kernels() {
    if (kernels)
        return;
    cuda_check(api->load_module(&kernels, am_rtx_output_ptx), "load RTX HDR output kernels");
    try {
        cuda_check(api->module_function(&pq_output, kernels, "am_rtx_pq_output"),
                   "resolve PQ output kernel");
        cuda_check(api->module_function(&sdr_output, kernels, "am_rtx_sdr_output"),
                   "resolve SDR output kernel");
    } catch (...) {
        api->unload_module(kernels);
        kernels = nullptr;
        throw;
    }
}

void Session::write_output(CudaOutput &destination) {
    load_kernels();
    unsigned width = input.width, height = input.height;
    unsigned target_pitch = static_cast<unsigned>(destination.pitch);
    int extent_x = static_cast<int>(width), extent_y = static_cast<int>(height);
    if (enabled) {
        // The model's packed words are read straight from its output image.
        const void *source = output.pixels;
        unsigned source_pitch = static_cast<unsigned>(output.pitch);
        float peak = static_cast<float>(parameters[3]);
        float gray = static_cast<float>(parameters[2]);
        void *arguments[] = {&source,   &source_pitch, &destination.pixels, &target_pitch,
                             &extent_x, &extent_y,     &peak,               &gray};
        cuda_check(api->launch(pq_output, blocks(width), blocks(height), 1, block_size,
                               block_size, 1, 0, stream, arguments, nullptr),
                   "launch PQ output");
    } else {
        // Conversion off: the RGBA8 model input still holds the sampled frame.
        const void *source = input.pixels;
        unsigned source_pitch = static_cast<unsigned>(input.pitch);
        float white = 100.0f + static_cast<float>(sdr_brightness);
        void *arguments[] = {&source,   &source_pitch, &destination.pixels, &target_pitch,
                             &extent_x, &extent_y,     &white};
        cuda_check(api->launch(sdr_output, blocks(width), blocks(height), 1, block_size,
                               block_size, 1, 0, stream, arguments, nullptr),
                   "launch SDR output");
    }
}

void Session::convert_to_cuda(CudaOutput &destination) {
    // Mapping orders the GL sampling draw before this stream. Everything after
    // it is CUDA on one stream: upload, model, output kernel, ready event.
    cuda_check(api->map(1, &input_resource, stream), "map effect input");
    try {
        copy_input();
        if (enabled)
            vfx_check(api->run_effect(effect, 1), "run TrueHDR");
        write_output(destination);
        cuda_check(api->record_event(destination.ready, stream), "record output ready");
    } catch (...) {
        if (api->unmap(1, &input_resource, stream) != CUDA_SUCCESS)
            std::abort();
        throw;
    }
    cuda_check(api->unmap(1, &input_resource, stream), "unmap effect input");
}

int Session::run_cuda(const AmaterasuDmaBufFrame &frame, CudaOutput &destination) {
    timing.begin(*api);
    surface.wait_acquire(frame.planes[0].acquire_fence_fd);
    timing.started();
    surface.capture(frame);
    timing.sampled();
    // Only the GL sampling draw reads the borrowed input; this fence covers it
    // and lets the caller keep the input until then without blocking.
    const int input_read = surface.complete();
    timing.model_queued(*api, stream, true);
    try {
        convert_to_cuda(destination);
    } catch (...) {
        if (input_read >= 0)
            ::close(input_read);
        throw;
    }
    timing.model_queued(*api, stream, false);
    timing.resumed();
    timing.finished();
    return input_read;
}
} // namespace rtx

namespace {
void report(char *buffer, size_t capacity, const std::exception &error) {
    if (capacity) {
        std::strncpy(buffer, error.what(), capacity - 1);
        buffer[capacity - 1] = 0;
    }
}
} // namespace

extern "C" int am_rtx_device_uuid(const char *root, int ordinal, uint8_t *uuid, char *error,
                                  size_t capacity) {
    try {
        auto api = rtx::Api::load(root);
        rtx::cuda_check(api->init(0), "initialize CUDA");
        CUdevice device = 0;
        rtx::cuda_check(api->device_get(&device, ordinal), "select CUDA device");
        CUuuid value{};
        rtx::cuda_check(api->device_uuid(&value, device), "query CUDA device UUID");
        std::memcpy(uuid, value.bytes, sizeof(value.bytes));
        return 0;
    } catch (const std::exception &failure) {
        report(error, capacity, failure);
        return -1;
    }
}

extern "C" void *am_rtx_cuda_output_open(void *context, uint64_t *pixels, uint32_t *pitch,
                                         void **ready, char *error, size_t capacity) {
    auto &session = *static_cast<rtx::Session *>(context);
    auto output = std::make_unique<rtx::CudaOutput>();
    try {
        rtx::Current current(*session.api, session.surface, session.context);
        rtx::cuda_check(session.api->allocate_pitch(&output->pixels, &output->pitch,
                                                    size_t(session.input.width) * 4,
                                                    session.input.height, 4),
                        "allocate CUDA output");
        rtx::cuda_check(
            session.api->create_event(&output->ready, CU_EVENT_DISABLE_TIMING),
            "create CUDA output event");
        if (output->pitch > UINT32_MAX)
            throw std::runtime_error("CUDA output pitch is too large");
        *pixels = output->pixels;
        *pitch = static_cast<uint32_t>(output->pitch);
        *ready = output->ready;
        return output.release();
    } catch (const std::exception &failure) {
        try {
            rtx::Current current(*session.api, session.surface, session.context);
            if (output->ready)
                session.api->destroy_event(output->ready);
            if (output->pixels)
                session.api->free_memory(output->pixels);
        } catch (...) {
        }
        report(error, capacity, failure);
        return nullptr;
    }
}

extern "C" void am_rtx_cuda_output_close(void *context, void *raw) {
    auto &session = *static_cast<rtx::Session *>(context);
    auto *output = static_cast<rtx::CudaOutput *>(raw);
    try {
        rtx::Current current(*session.api, session.surface, session.context);
        // Queued writes into this slot must finish before its memory is freed.
        if (session.api->synchronize_stream(session.stream) != CUDA_SUCCESS)
            std::abort();
        session.api->destroy_event(output->ready);
        session.api->free_memory(output->pixels);
    } catch (...) {
        std::abort();
    }
    delete output;
}

extern "C" int am_rtx_process_cuda(void *context, const AmaterasuDmaBufFrame *frame, void *output,
                                   int *input_fence, char *error, size_t capacity) {
    auto &session = *static_cast<rtx::Session *>(context);
    *input_fence = -1;
    try {
        rtx::Current current(*session.api, session.surface, session.context);
        try {
            *input_fence = session.run_cuda(*frame, *static_cast<rtx::CudaOutput *>(output));
        } catch (...) {
            // No input borrow may escape on failure: retire queued GPU work.
            if (session.api->synchronize_context() != CUDA_SUCCESS)
                std::abort();
            glFinish();
            throw;
        }
        return 0;
    } catch (const std::exception &failure) {
        report(error, capacity, failure);
        return -1;
    }
}
