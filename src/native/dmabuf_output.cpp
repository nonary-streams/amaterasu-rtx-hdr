// DMA-BUF resource output. The converted X2B10G10R10 frame is produced in a
// CUDA slot exactly as for the CUDA offer, then published unchanged into an
// exported XB30 buffer for consumers that import DMA-BUF (Vulkan, GL, KMS).
// The GL pass is ordered after CUDA by the interop unmap, and its native fence
// is handed to the reader as the plane's acquire fence; no thread waits here.
#include "internal.hpp"
#include <cstring>
#include <drm_fourcc.h>
#include <fcntl.h>
#include <unistd.h>
#include <vector>

namespace rtx {
namespace {
void gl_check(const char *operation) {
    if (glGetError() != GL_NO_ERROR)
        throw std::runtime_error(operation);
}

GLuint compile(GLenum kind, const char *source) {
    GLuint shader = glCreateShader(kind);
    glShaderSource(shader, 1, &source, nullptr);
    glCompileShader(shader);
    GLint compiled = 0;
    glGetShaderiv(shader, GL_COMPILE_STATUS, &compiled);
    if (!compiled) {
        glDeleteShader(shader);
        throw std::runtime_error("DMA-BUF publication shader compilation failed");
    }
    return shader;
}

// Each packed word is stored as the same three 10-bit components; the target
// surface quantizes value/1023 back to the identical code value.
GLuint publication_program() {
    const char *vertex = "#version 330\n"
                         "void main(){ vec2 p=vec2((gl_VertexID<<1)&2,gl_VertexID&2);"
                         "gl_Position=vec4(p*2.0-1.0,0,1);}";
    const char *fragment = "#version 330\n"
                           "uniform usampler2D source; out vec4 color;\n"
                           "void main(){ uint w=texelFetch(source,ivec2(gl_FragCoord.xy),0).r;\n"
                           "color=vec4(float(w&1023u),float((w>>10)&1023u),"
                           "float((w>>20)&1023u),1023.0)/1023.0;}";
    GLuint vs = compile(GL_VERTEX_SHADER, vertex), fs = 0;
    try {
        fs = compile(GL_FRAGMENT_SHADER, fragment);
    } catch (...) {
        glDeleteShader(vs);
        throw;
    }
    GLuint program = glCreateProgram();
    glAttachShader(program, vs);
    glAttachShader(program, fs);
    glLinkProgram(program);
    glDeleteShader(vs);
    glDeleteShader(fs);
    GLint linked = 0;
    glGetProgramiv(program, GL_LINK_STATUS, &linked);
    if (!linked) {
        glDeleteProgram(program);
        throw std::runtime_error("DMA-BUF publication shader link failed");
    }
    glUseProgram(program);
    glUniform1i(glGetUniformLocation(program, "source"), 0);
    return program;
}


void destroy_target(Surface &surface, DmaBufTarget &target) {
    if (target.fence >= 0)
        ::close(target.fence);
    if (target.fbo)
        glDeleteFramebuffers(1, &target.fbo);
    if (target.texture)
        glDeleteTextures(1, &target.texture);
    if (target.image != EGL_NO_IMAGE_KHR)
        surface.destroy_image(surface.display, target.image);
    if (target.frame.planes[0].fd >= 0)
        ::close(target.frame.planes[0].fd);
    if (target.buffer)
        gbm_bo_destroy(target.buffer);
    target = DmaBufTarget{};
}
} // namespace

static void report(char *buffer, size_t capacity, const std::exception &error) {
    if (capacity) {
        std::strncpy(buffer, error.what(), capacity - 1);
        buffer[capacity - 1] = 0;
    }
}

void Session::attach_dmabuf(CudaOutput &output, uint64_t modifier) {
    auto target = std::make_unique<DmaBufTarget>();
    for (auto &plane : target->frame.planes)
        plane = {-1, -1, 0, 0};
    try {
        target->buffer = gbm_bo_create_with_modifiers(surface.gbm, input.width, input.height,
                                                      DRM_FORMAT_XBGR2101010, &modifier, 1);
        if (!target->buffer || gbm_bo_get_plane_count(target->buffer) != 1 ||
            gbm_bo_get_modifier(target->buffer) != modifier)
            throw std::runtime_error("DMA-BUF output allocation failed for the offered modifier");
        auto &frame = target->frame;
        frame.width = input.width;
        frame.height = input.height;
        frame.fourcc = DRM_FORMAT_XBGR2101010;
        frame.plane_count = 1;
        frame.modifier = modifier;
        frame.color = AMATERASU_DMABUF_COLOR_BT2020_PQ;
        frame.planes[0].fd = gbm_bo_get_fd(target->buffer);
        frame.planes[0].pitch = gbm_bo_get_stride(target->buffer);
        frame.planes[0].offset = gbm_bo_get_offset(target->buffer, 0);
        if (frame.planes[0].fd < 0)
            throw std::runtime_error("DMA-BUF output export failed");
        target->image = surface.import(frame);
        glGenTextures(1, &target->texture);
        glBindTexture(GL_TEXTURE_2D, target->texture);
        surface.bind_image(GL_TEXTURE_2D, target->image);
        glGenFramebuffers(1, &target->fbo);
        glBindFramebuffer(GL_FRAMEBUFFER, target->fbo);
        glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                               target->texture, 0);
        if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE)
            throw std::runtime_error("DMA-BUF output is not renderable");
        gl_check("DMA-BUF output import failed");
    } catch (...) {
        destroy_target(surface, *target);
        throw;
    }
    output.dmabuf = std::move(target);
}

int Session::publish_dmabuf(CudaOutput &output) {
    auto &target = *output.dmabuf;
    auto &staging = publisher;
    if (!staging.staging || staging.width != input.width || staging.height != input.height) {
        close_publisher();
        glGenTextures(1, &staging.staging);
        glBindTexture(GL_TEXTURE_2D, staging.staging);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        glTexImage2D(GL_TEXTURE_2D, 0, GL_R32UI, input.width, input.height, 0, GL_RED_INTEGER,
                     GL_UNSIGNED_INT, nullptr);
        gl_check("DMA-BUF staging allocation failed");
        cuda_check(api->register_image(&staging.resource, staging.staging, GL_TEXTURE_2D,
                                       CU_GRAPHICS_REGISTER_FLAGS_WRITE_DISCARD),
                   "register DMA-BUF staging");
        staging.program = publication_program();
        staging.width = input.width;
        staging.height = input.height;
    }
    // The CUDA write into this slot precedes the copy on the same stream; the
    // unmap orders the copy before the GL pass that reads the staging texture.
    cuda_check(api->map(1, &staging.resource, stream), "map DMA-BUF staging");
    try {
        CUarray array = nullptr;
        cuda_check(api->mapped_array(&array, staging.resource, 0, 0), "resolve DMA-BUF staging");
        CUDA_MEMCPY2D copy{};
        copy.srcMemoryType = CU_MEMORYTYPE_DEVICE;
        copy.srcDevice = output.pixels;
        copy.srcPitch = output.pitch;
        copy.dstMemoryType = CU_MEMORYTYPE_ARRAY;
        copy.dstArray = array;
        copy.WidthInBytes = size_t(input.width) * 4;
        copy.Height = input.height;
        cuda_check(api->copy(&copy, stream), "stage DMA-BUF output");
    } catch (...) {
        api->unmap(1, &staging.resource, stream);
        throw;
    }
    cuda_check(api->unmap(1, &staging.resource, stream), "unmap DMA-BUF staging");
    glBindFramebuffer(GL_FRAMEBUFFER, target.fbo);
    glViewport(0, 0, input.width, input.height);
    glUseProgram(staging.program);
    glActiveTexture(GL_TEXTURE0);
    glBindTexture(GL_TEXTURE_2D, staging.staging);
    glBindVertexArray(surface.vao);
    glDrawArrays(GL_TRIANGLES, 0, 3);
    gl_check("DMA-BUF publication failed");
    const int fence = surface.complete();
    if (target.fence >= 0)
        ::close(target.fence);
    // Without a native fence complete() finished the work synchronously.
    target.fence = fence;
    target.frame.planes[0].acquire_fence_fd = fence;
    return fence;
}

void Session::close_publisher() {
    if (publisher.resource)
        cuda_check(api->unregister_image(publisher.resource), "retire DMA-BUF staging");
    if (publisher.staging)
        glDeleteTextures(1, &publisher.staging);
    if (publisher.program)
        glDeleteProgram(publisher.program);
    publisher = DmaBufPublisher{};
}

void release_dmabuf(Surface &surface, CudaOutput &output) {
    if (!output.dmabuf)
        return;
    // A reader may still hold the fence of the last publication; the slot is
    // released only after every reference, so the GL work can be retired here.
    glFinish();
    destroy_target(surface, *output.dmabuf);
    output.dmabuf.reset();
}
} // namespace rtx

extern "C" int am_rtx_dmabuf_modifiers(const char *root, int ordinal, uint32_t fourcc,
                                       uint64_t *modifiers, size_t capacity, size_t *count,
                                       char *error, size_t error_capacity) {
    *count = 0;
    try {
        auto api = rtx::Api::load(root);
        rtx::cuda_check(api->init(0), "initialize CUDA");
        CUdevice device = 0;
        rtx::cuda_check(api->device_get(&device, ordinal), "select CUDA device");
        rtx::Surface surface;
        try {
            surface.open(device);
            auto query = reinterpret_cast<PFNEGLQUERYDMABUFMODIFIERSEXTPROC>(
                eglGetProcAddress("eglQueryDmaBufModifiersEXT"));
            if (!query)
                throw std::runtime_error("EGL DMA-BUF modifier query is unavailable");
            EGLint available = 0;
            if (!query(surface.display, static_cast<EGLint>(fourcc), 0, nullptr, nullptr,
                       &available) ||
                available <= 0)
                throw std::runtime_error("the GPU reports no modifier for the output format");
            std::vector<EGLuint64KHR> listed(available);
            std::vector<EGLBoolean> external(available);
            if (!query(surface.display, static_cast<EGLint>(fourcc), available, listed.data(),
                       external.data(), &available))
                throw std::runtime_error("EGL DMA-BUF modifier query failed");
            for (EGLint index = 0; index < available && *count < capacity; ++index) {
                // External-only modifiers can be sampled but not rendered into.
                if (!external[index])
                    modifiers[(*count)++] = listed[index];
            }
        } catch (...) {
            if (surface.context != EGL_NO_CONTEXT)
                eglDestroyContext(surface.display, surface.context);
            if (surface.display != EGL_NO_DISPLAY)
                eglTerminate(surface.display);
            if (surface.gbm)
                gbm_device_destroy(surface.gbm);
            if (surface.render_fd >= 0)
                ::close(surface.render_fd);
            throw;
        }
        if (surface.context != EGL_NO_CONTEXT)
            eglDestroyContext(surface.display, surface.context);
        eglTerminate(surface.display);
        gbm_device_destroy(surface.gbm);
        ::close(surface.render_fd);
        return 0;
    } catch (const std::exception &failure) {
        rtx::report(error, error_capacity, failure);
        return -1;
    }
}

extern "C" int am_rtx_cuda_output_attach_dmabuf(void *context, void *raw, uint64_t modifier,
                                                AmaterasuDmaBufFrame *frame, char *error,
                                                size_t capacity) {
    auto &session = *static_cast<rtx::Session *>(context);
    auto &output = *static_cast<rtx::CudaOutput *>(raw);
    try {
        rtx::Current current(*session.api, session.surface, session.context);
        session.attach_dmabuf(output, modifier);
        *frame = output.dmabuf->frame;
        return 0;
    } catch (const std::exception &failure) {
        rtx::report(error, capacity, failure);
        return -1;
    }
}

extern "C" int am_rtx_publish_dmabuf(void *context, void *raw, int *fence, char *error,
                                     size_t capacity) {
    auto &session = *static_cast<rtx::Session *>(context);
    auto &output = *static_cast<rtx::CudaOutput *>(raw);
    *fence = -1;
    try {
        if (!output.dmabuf)
            throw std::runtime_error("CUDA slot has no DMA-BUF target");
        rtx::Current current(*session.api, session.surface, session.context);
        try {
            *fence = session.publish_dmabuf(output);
        } catch (...) {
            // No partially published frame may be read: retire queued work.
            if (session.api->synchronize_context() != CUDA_SUCCESS)
                std::abort();
            glFinish();
            throw;
        }
        return 0;
    } catch (const std::exception &failure) {
        rtx::report(error, capacity, failure);
        return -1;
    }
}
