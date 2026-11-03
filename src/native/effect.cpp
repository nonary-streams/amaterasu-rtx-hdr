#include "internal.hpp"
#include <unistd.h>

namespace rtx {
namespace {
void copy_texture(Api &api, CUstream stream, CUgraphicsResource resource, Image &image,
                  bool upload) {
    cuda_check(api.map(1, &resource, stream), "map effect texture");
    try {
        CUarray array = nullptr;
        cuda_check(api.mapped_array(&array, resource, 0, 0), "resolve effect texture");
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
    } catch (...) {
        if (api.unmap(1, &resource, stream) != CUDA_SUCCESS)
            std::abort();
        throw;
    }
    cuda_check(api.unmap(1, &resource, stream), "unmap effect texture");
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
        vfx_check(api->allocate(&input, width, height, 6, 1, 0, 1, 1), "allocate RGBA8 input");
        vfx_check(api->allocate(&output, width, height, 13, 11, 0, 1, 1),
                  "allocate RGB10A2 output");
        vfx_check(api->create_effect("TrueHDR", &effect), "create TrueHDR");
        vfx_check(api->set_stream(effect, "CudaStream", stream), "set effect stream");
        vfx_check(api->set_image(effect, "SrcImage0", &input), "set effect input");
        vfx_check(api->set_image(effect, "DstImage0", &output), "set effect output");
        const char *names[] = {"Contrast", "Saturation", "MiddleGray", "Luminance", "DebandingOff"};
        for (size_t index = 0; index < 5; ++index)
            vfx_check(api->set_u32(effect, names[index], parameters[index]), names[index]);
        vfx_check(api->load_effect(effect), "load TrueHDR model");
        cuda_check(api->register_image(&input_resource, surface.input, GL_TEXTURE_2D,
                                       CU_GRAPHICS_REGISTER_FLAGS_READ_ONLY),
                   "register effect input");
        cuda_check(api->register_image(&output_resource, surface.output, GL_TEXTURE_2D,
                                       CU_GRAPHICS_REGISTER_FLAGS_WRITE_DISCARD),
                   "register effect output");
    } catch (...) {
        close();
        throw;
    }
}

void Session::run(const AmaterasuDmaBufFrame &frame, Output &destination) {
    surface.capture(frame);
    copy_texture(*api, stream, input_resource, input, true);
    vfx_check(api->run_effect(effect, 0), "run TrueHDR");
    copy_texture(*api, stream, output_resource, output, false);
    cuda_check(api->synchronize_stream(stream), "complete TrueHDR conversion");
    glBindFramebuffer(GL_FRAMEBUFFER, destination.fbo);
    glViewport(0, 0, input.width, input.height);
    glUseProgram(surface.output_program);
    glBindVertexArray(surface.vao);
    glActiveTexture(GL_TEXTURE0);
    glBindTexture(GL_TEXTURE_2D, surface.output);
    glDrawArrays(GL_TRIANGLES, 0, 3);
    glFinish();
    if (glGetError() != GL_NO_ERROR)
        throw std::runtime_error("HDR output copy failed");
}

void Session::close() {
    if (context && surface.context != EGL_NO_CONTEXT) {
        try {
            Current current(*api, surface, context);
            if (stream)
                cuda_check(api->synchronize_stream(stream), "retire effect stream");
            if (input_resource)
                cuda_check(api->unregister_image(input_resource), "retire effect input");
            if (output_resource)
                cuda_check(api->unregister_image(output_resource), "retire effect output");
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
