// Opt-in GPU qualification. Readback is confined to this test harness; the
// production processing path never transfers pixels through host memory.
#include "internal.hpp"
#include <unistd.h>
namespace rtx {
struct Output {
    gbm_bo *buffer = nullptr;
    EGLImageKHR image = EGL_NO_IMAGE_KHR;
    GLuint texture = 0, fbo = 0;
    AmaterasuDmaBufFrame frame{};
    void open(Surface &surface, uint32_t width, uint32_t height,
              uint32_t format);
    void close(Surface &surface);
};

void Output::open(Surface &surface, uint32_t width, uint32_t height, uint32_t format) {
    for (auto &plane : frame.planes)
        plane = {-1, -1, 0, 0};
    buffer = gbm_bo_create(surface.gbm, width, height, format, GBM_BO_USE_RENDERING);
    if (!buffer || gbm_bo_get_plane_count(buffer) != 1)
        throw std::runtime_error("HDR output allocation failed");
    frame.width = width;
    frame.height = height;
    frame.fourcc = format;
    frame.plane_count = 1;
    frame.color = AMATERASU_DMABUF_COLOR_BT2020_PQ;
    frame.modifier = gbm_bo_get_modifier(buffer);
    frame.planes[0].fd = gbm_bo_get_fd(buffer);
    frame.planes[0].pitch = gbm_bo_get_stride(buffer);
    frame.planes[0].offset = gbm_bo_get_offset(buffer, 0);
    if (frame.planes[0].fd < 0)
        throw std::runtime_error("HDR output export failed");
    image = surface.import(frame);
    glGenTextures(1, &texture);
    glBindTexture(GL_TEXTURE_2D, texture);
    surface.bind_image(GL_TEXTURE_2D, image);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, texture, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE || glGetError() != GL_NO_ERROR)
        throw std::runtime_error("test source framebuffer failed");
}
void Output::close(Surface &surface) {
    // A rejected delivery can release the last reference before anyone waited
    // on the output fence. Retire this context's queued writes before freeing.
    glFinish();
    glDeleteFramebuffers(1, &fbo);
    glDeleteTextures(1, &texture);
    if (image != EGL_NO_IMAGE_KHR)
        surface.destroy_image(surface.display, image);
    if (frame.planes[0].fd >= 0)
        ::close(frame.planes[0].fd);
    if (buffer)
        gbm_bo_destroy(buffer);
}
}


extern "C" void *am_rtx_test_source_format(void *raw, AmaterasuDmaBufFrame *frame,
                                           uint32_t format) {
    auto &session = *static_cast<rtx::Session *>(raw);
    try {
        auto output = std::make_unique<rtx::Output>();
        rtx::Current current(*session.api, session.surface, session.context);
        try {
            output->open(session.surface, session.input.width, session.input.height,
                         format);
            glBindFramebuffer(GL_FRAMEBUFFER, output->fbo);
            glClearColor(0, 0, 0, 1);
            glClear(GL_COLOR_BUFFER_BIT);
            glEnable(GL_SCISSOR_TEST);
            glScissor(0, 0, session.input.width / 2, session.input.height);
            glClearColor(0.7f, 0.3f, 0.1f, 1);
            glClear(GL_COLOR_BUFFER_BIT);
            glScissor(session.input.width / 2, 0, session.input.width / 4, session.input.height);
            glClearColor(1, 1, 1, 1);
            glClear(GL_COLOR_BUFFER_BIT);
            glDisable(GL_SCISSOR_TEST);
            glFinish();
            if (glGetError() != GL_NO_ERROR)
                throw std::runtime_error("test source rendering failed");
        } catch (...) {
            output->close(session.surface);
            throw;
        }
        *frame = output->frame;
        frame->color = AMATERASU_DMABUF_COLOR_SRGB;
        return output.release();
    } catch (...) {
        return nullptr;
    }
}
extern "C" int am_rtx_test_read_cuda(void *raw, uint64_t pointer, uint32_t pitch,
                                     uint32_t *pixels, size_t length) {
    auto &session = *static_cast<rtx::Session *>(raw);
    if (length != static_cast<size_t>(session.input.width) * session.input.height)
        return -1;
    try {
        rtx::Current current(*session.api, session.surface, session.context);
        // Slot writes are queued on the session stream; read after they finish.
        rtx::cuda_check(session.api->synchronize_context(), "finish CUDA test output");
        CUDA_MEMCPY2D copy{};
        copy.srcMemoryType = CU_MEMORYTYPE_DEVICE;
        copy.srcDevice = static_cast<CUdeviceptr>(pointer);
        copy.srcPitch = pitch;
        copy.dstMemoryType = CU_MEMORYTYPE_HOST;
        copy.dstHost = pixels;
        copy.dstPitch = session.input.width * 4;
        copy.WidthInBytes = copy.dstPitch;
        copy.Height = session.input.height;
        rtx::cuda_check(session.api->copy(&copy, session.stream), "read CUDA output");
        rtx::cuda_check(session.api->synchronize_stream(session.stream), "complete CUDA read");
        return 0;
    } catch (...) {
        return -1;
    }
}
extern "C" int am_rtx_test_fence(void *raw) {
    auto &session = *static_cast<rtx::Session *>(raw);
    try {
        rtx::Current current(*session.api, session.surface, session.context);
        auto create =
            reinterpret_cast<PFNEGLCREATESYNCKHRPROC>(eglGetProcAddress("eglCreateSyncKHR"));
        auto duplicate = reinterpret_cast<PFNEGLDUPNATIVEFENCEFDANDROIDPROC>(
            eglGetProcAddress("eglDupNativeFenceFDANDROID"));
        auto destroy =
            reinterpret_cast<PFNEGLDESTROYSYNCKHRPROC>(eglGetProcAddress("eglDestroySyncKHR"));
        if (!create || !duplicate || !destroy)
            return -1;
        EGLSyncKHR sync = create(session.surface.display, EGL_SYNC_NATIVE_FENCE_ANDROID, nullptr);
        if (sync == EGL_NO_SYNC_KHR)
            return -1;
        glFlush();
        int fd = duplicate(session.surface.display, sync);
        destroy(session.surface.display, sync);
        return fd;
    } catch (...) {
        return -1;
    }
}

extern "C" void am_rtx_test_source_close(void *raw, void *source) {
    auto &session=*static_cast<rtx::Session *>(raw);
    auto *output=static_cast<rtx::Output *>(source);
    try {rtx::Current current(*session.api, session.surface,session.context);output->close(session.surface);}catch(...){std::abort();}
    delete output;
}

namespace {
struct InputCopyProbe {
    unsigned step = 0;
    unsigned failure = 0;
    bool mapped = false;
    bool valid = true;
};
thread_local InputCopyProbe *input_copy_probe = nullptr;
CUresult CUDAAPI probe_map(unsigned count, CUgraphicsResource *resources, CUstream) {
    auto &probe = *input_copy_probe;
    probe.valid &= count == 1 && resources != nullptr && ++probe.step == 1;
    if (probe.failure == 2) return CUDA_ERROR_INVALID_VALUE;
    probe.mapped = true;
    return CUDA_SUCCESS;
}
CUresult CUDAAPI probe_array(CUarray *array, CUgraphicsResource, unsigned index, unsigned level) {
    auto &probe = *input_copy_probe;
    probe.valid &= probe.mapped && ++probe.step == 2 && index == 0 && level == 0;
    *array = reinterpret_cast<CUarray>(0x1000);
    return CUDA_SUCCESS;
}
CUresult CUDAAPI probe_copy(const CUDA_MEMCPY2D *copy, CUstream) {
    auto &probe = *input_copy_probe;
    probe.valid &= probe.mapped && ++probe.step == 3 && copy != nullptr &&
        copy->srcMemoryType == CU_MEMORYTYPE_ARRAY && copy->dstMemoryType == CU_MEMORYTYPE_DEVICE &&
        copy->WidthInBytes == 8 && copy->Height == 1;
    return probe.failure == 1 ? CUDA_ERROR_INVALID_VALUE : CUDA_SUCCESS;
}
CUresult CUDAAPI probe_unmap(unsigned count, CUgraphicsResource *, CUstream) {
    auto &probe = *input_copy_probe;
    probe.valid &= probe.mapped && count == 1 && ++probe.step == 4;
    probe.mapped = false;
    return CUDA_SUCCESS;
}
}

// Exercise the actual production copy operation without a GPU. Returning to
// the caller that queues inference must release the mapped graphics resource,
// including on a failed copy; a failed map must not unmap an unowned resource.
extern "C" int am_rtx_test_input_copy_scope(unsigned failure) {
    InputCopyProbe probe;
    probe.failure = failure;
    input_copy_probe = &probe;
    rtx::Api api{};
    api.map = probe_map;
    api.mapped_array = probe_array;
    api.copy = probe_copy;
    api.unmap = probe_unmap;
    rtx::Image image{};
    image.width = 2;
    image.height = 1;
    image.pitch = 8;
    image.pixels = reinterpret_cast<void *>(0x2000);
    bool threw = false;
    try { rtx::copy_graphics_input(api, nullptr, nullptr, image); }
    catch (...) { threw = true; }
    input_copy_probe = nullptr;
    return probe.valid && !probe.mapped && threw == (failure != 0) &&
        probe.step == (failure == 2 ? 1u : 4u) ? 0 : -1;
}
