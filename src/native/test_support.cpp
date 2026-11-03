// Opt-in GPU qualification. Readback is confined to this test harness; the
// production processing path never transfers pixels through host memory.
#include "internal.hpp"

extern "C" void *am_rtx_test_source(void *raw, AmaterasuDmaBufFrame *frame) {
    auto &session = *static_cast<rtx::Session *>(raw);
    try {
        auto output = std::make_unique<rtx::Output>();
        rtx::Current current(*session.api, session.surface, session.context);
        try {
            output->open(session.surface, session.input.width, session.input.height,
                         GBM_FORMAT_ABGR8888);
            glBindFramebuffer(GL_FRAMEBUFFER, output->fbo);
            glClearColor(0, 0, 0, 1);
            glClear(GL_COLOR_BUFFER_BIT);
            glEnable(GL_SCISSOR_TEST);
            glScissor(0, 0, session.input.width / 2, session.input.height);
            glClearColor(0.7f, 0.3f, 0.1f, 1);
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
extern "C" int am_rtx_test_read(void *raw, void *surface, uint32_t *pixels, size_t length) {
    auto &session = *static_cast<rtx::Session *>(raw);
    auto &output = *static_cast<rtx::Output *>(surface);
    if (length != static_cast<size_t>(session.input.width) * session.input.height)
        return -1;
    try {
        rtx::Current current(*session.api, session.surface, session.context);
        glBindFramebuffer(GL_READ_FRAMEBUFFER, output.fbo);
        glReadPixels(0, 0, session.input.width, session.input.height, GL_RGBA,
                     GL_UNSIGNED_INT_2_10_10_10_REV, pixels);
        return glGetError() == GL_NO_ERROR ? 0 : -1;
    } catch (...) {
        return -1;
    }
}
extern "C" int am_rtx_test_read_vendor(void *raw, uint32_t *pixels, size_t length) {
    auto &session = *static_cast<rtx::Session *>(raw);
    if (length != static_cast<size_t>(session.input.width) * session.input.height)
        return -1;
    try {
        rtx::Current current(*session.api, session.surface, session.context);
        CUDA_MEMCPY2D copy{};
        copy.srcMemoryType = CU_MEMORYTYPE_DEVICE;
        copy.srcDevice = reinterpret_cast<CUdeviceptr>(session.output.pixels);
        copy.srcPitch = session.output.pitch;
        copy.dstMemoryType = CU_MEMORYTYPE_HOST;
        copy.dstHost = pixels;
        copy.dstPitch = session.output.width * 4;
        copy.WidthInBytes = copy.dstPitch;
        copy.Height = session.output.height;
        rtx::cuda_check(session.api->copy(&copy, session.stream), "read vendor test output");
        rtx::cuda_check(session.api->synchronize_stream(session.stream),
                        "complete vendor test read");
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
