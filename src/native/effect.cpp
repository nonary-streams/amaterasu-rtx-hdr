#include "internal.hpp"
#include <unistd.h>
#include <algorithm>
#include <cmath>

namespace rtx {
namespace {
void copy_array(Api &api, CUstream stream, CUarray array, Image &image, bool upload) {
    CUDA_MEMCPY2D copy{};
    copy.WidthInBytes = image.width * 4;
    copy.Height = image.height;
    if (upload) {
        copy.srcMemoryType = CU_MEMORYTYPE_ARRAY;
        copy.srcArray = array;
        copy.dstMemoryType = CU_MEMORYTYPE_DEVICE;
        copy.dstDevice = reinterpret_cast<CUdeviceptr>(image.pixels);
        copy.dstPitch = image.pitch;
    } else {
        copy.srcMemoryType = CU_MEMORYTYPE_DEVICE;
        copy.srcDevice = reinterpret_cast<CUdeviceptr>(image.pixels);
        copy.srcPitch = image.pitch;
        copy.dstMemoryType = CU_MEMORYTYPE_ARRAY;
        copy.dstArray = array;
    }
    cuda_check(api.copy(&copy, stream), "copy effect texture");
}

CUarray mapped_array(Api &api, CUgraphicsResource resource) {
    CUarray array = nullptr;
    cuda_check(api.mapped_array(&array, resource, 0, 0), "resolve effect texture");
    return array;
}
} // namespace

Session::Session(const std::string &root, int ordinal, uint32_t width, uint32_t height,
                 const uint32_t *parameters)
    : api(Api::load(root)) {
    try {
        cuda_check(api->init(0), "initialize CUDA");
        cuda_check(api->device_get(&device, ordinal), "select CUDA device");
        cuda_check(api->retain_context(&context, device), "retain CUDA context");
        retained_context = true;
        surface.open(device);
        Current current(*api, surface, context);
        surface.prepare(width, height);
        // VFX/NGX also performs initialization on CUDA's legacy default stream.
        // Match NvVFX_CudaStreamCreate's default-stream ordering semantics.
        cuda_check(api->create_stream(&stream, CU_STREAM_DEFAULT), "create effect stream");
        timing.open(*api);
        vfx_check(api->allocate(&input, width, height, 6, 1, 0, 1, 1), "allocate RGBA8 input");
        vfx_check(api->allocate(&output, width, height, 13, 11, 0, 1, 1),
                  "allocate RGB10A2 output");
        effect = create_configured_effect(parameters);
        std::copy_n(parameters, 5, this->parameters.begin());
        cuda_check(api->register_image(&input_resource, surface.input, GL_TEXTURE_2D,
                                       CU_GRAPHICS_REGISTER_FLAGS_READ_ONLY),
                   "register effect input");
    } catch (...) {
        close();
        throw;
    }
}

void *Session::create_configured_effect(const uint32_t *values) {
    // The Linux runtime's packed output saturates near 1000 nits. Above that
    // level, run the model at 1000 and expand the encoded light in the output
    // shader. Reduce the model's gray target to preserve the requested midtone.
    std::array<uint32_t, 5> native;
    std::copy_n(values, 5, native.begin());
    if (native[3] > 1000) {
        native[2] = std::clamp(static_cast<uint32_t>(std::lround(
                                   static_cast<double>(values[2]) * 1000.0 / values[3])),
                               10u, 100u);
        native[3] = 1000;
    }
    void *candidate = nullptr;
    vfx_check(api->create_effect("TrueHDR", &candidate), "create TrueHDR");
    try {
        vfx_check(api->set_stream(candidate, "CudaStream", stream), "set effect stream");
        vfx_check(api->set_image(candidate, "SrcImage0", &input), "set effect input");
        vfx_check(api->set_image(candidate, "DstImage0", &output), "set effect output");
        const char *names[] = {"Contrast", "Saturation", "MiddleGray", "Luminance", "DebandingOff"};
        for (size_t index = 0; index < native.size(); ++index)
            vfx_check(api->set_u32(candidate, names[index], native[index]), names[index]);
        vfx_check(api->load_effect(candidate), "load TrueHDR model");
    } catch (...) {
        api->destroy_effect(candidate);
        throw;
    }
    return candidate;
}

void Session::configure(const uint32_t *values, bool active, uint32_t brightness) {
    if (!values || values[0] > 200 || values[1] > 200 || values[2] < 10 ||
        values[2] > 100 || values[3] < 400 || values[3] > 2000 || values[4] > 1 ||
        brightness > 100)
        throw std::runtime_error("invalid TrueHDR settings");
    if (!std::equal(parameters.begin(), parameters.end(), values)) {
        void *candidate = create_configured_effect(values);
        auto *previous = effect;
        // Earlier frames may still run the previous model asynchronously.
        // Drain them before the vendor releases that effect's resources.
        try {
            cuda_check(api->synchronize_stream(stream), "drain previous TrueHDR");
        } catch (...) {
            api->destroy_effect(candidate);
            throw;
        }
        effect = candidate;
        std::copy_n(values, 5, parameters.begin());
        api->destroy_effect(previous);
    }
    enabled = active;
    sdr_brightness = brightness;
}

// The input texture must be mapped on this session's stream.
void Session::copy_input() {
    copy_array(*api, stream, mapped_array(*api, input_resource), input, true);
}

void Session::close() {
    if (context && surface.context != EGL_NO_CONTEXT) {
        try {
            Current current(*api, surface, context);
            if (stream)
                cuda_check(api->synchronize_stream(stream), "retire effect stream");
            timing.close(*api);
            if (upload)
                api->free_memory(upload);
            if (kernels)
                api->unload_module(kernels);
            if (input_resource)
                cuda_check(api->unregister_image(input_resource), "retire effect input");
            if (effect)
                api->destroy_effect(effect);
            if (input.pixels)
                api->deallocate(&input);
            if (output.pixels)
                api->deallocate(&output);
            if (stream)
                cuda_check(api->destroy_stream(stream), "destroy effect stream");
            surface.close();
        } catch (...) {
            std::abort();
        }
        eglDestroyContext(surface.display, surface.context);
    }
    if (surface.display != EGL_NO_DISPLAY)
        eglTerminate(surface.display);
    if (surface.gbm)
        gbm_device_destroy(surface.gbm);
    if (surface.render_fd >= 0)
        ::close(surface.render_fd);
    if (retained_context && api->release_context(device) != CUDA_SUCCESS)
        std::abort();
}
Session::~Session() { close(); }
} // namespace rtx
