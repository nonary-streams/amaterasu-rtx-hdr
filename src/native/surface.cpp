#include "internal.hpp"
#include <cstring>
#include <drm_fourcc.h>
#include <fcntl.h>
#include <unistd.h>
#include <vector>

namespace rtx {
namespace {
bool extension(const char *list, const char *name) {
    if (!list)
        return false;
    const size_t size = std::strlen(name);
    for (auto *cursor = list; (cursor = std::strstr(cursor, name)); cursor += size) {
        if ((cursor == list || cursor[-1] == ' ') && (cursor[size] == 0 || cursor[size] == ' '))
            return true;
    }
    return false;
}
void gl_check(const char *operation) {
    if (glGetError() != GL_NO_ERROR)
        throw std::runtime_error(operation);
}
GLuint shader(GLenum kind, const char *source) {
    GLuint result = glCreateShader(kind);
    glShaderSource(result, 1, &source, nullptr);
    glCompileShader(result);
    GLint compiled = 0;
    glGetShaderiv(result, GL_COMPILE_STATUS, &compiled);
    if (!compiled) {
        glDeleteShader(result);
        throw std::runtime_error("effect shader compilation failed");
    }
    return result;
}
void framebuffer(GLuint texture, GLuint &fbo) {
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, texture, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE)
        throw std::runtime_error("effect framebuffer is incomplete");
}
void texture(GLuint &name, uint32_t width, uint32_t height) {
    glGenTextures(1, &name);
    glBindTexture(GL_TEXTURE_2D, name);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, width, height, 0, GL_RGBA, GL_UNSIGNED_BYTE, nullptr);
    gl_check("effect texture allocation failed");
}
} // namespace

void Surface::open(CUdevice cuda_device) {
    auto query_devices =
        reinterpret_cast<PFNEGLQUERYDEVICESEXTPROC>(eglGetProcAddress("eglQueryDevicesEXT"));
    auto query_attribute = reinterpret_cast<PFNEGLQUERYDEVICEATTRIBEXTPROC>(
        eglGetProcAddress("eglQueryDeviceAttribEXT"));
    auto query_string = reinterpret_cast<PFNEGLQUERYDEVICESTRINGEXTPROC>(
        eglGetProcAddress("eglQueryDeviceStringEXT"));
    auto platform = reinterpret_cast<PFNEGLGETPLATFORMDISPLAYEXTPROC>(
        eglGetProcAddress("eglGetPlatformDisplayEXT"));
    if (!query_devices || !query_attribute || !query_string || !platform)
        throw std::runtime_error("EGL device selection is unavailable");
    EGLDeviceEXT devices[16];
    EGLint count = 0;
    const char *node = nullptr;
    if (!query_devices(16, devices, &count))
        throw std::runtime_error("EGL enumeration failed");
    for (int index = 0; index < count && index < 16; ++index) {
        auto *extensions = query_string(devices[index], EGL_EXTENSIONS);
        EGLAttrib device = -1;
        if (extension(extensions, "EGL_NV_device_cuda") &&
            extension(extensions, "EGL_EXT_device_drm_render_node") &&
            query_attribute(devices[index], EGL_CUDA_DEVICE_NV, &device) && device == cuda_device) {
            node = query_string(devices[index], EGL_DRM_RENDER_NODE_FILE_EXT);
            break;
        }
    }
    if (!node)
        throw std::runtime_error("no DRM render node matches the selected CUDA device");
    render_fd = ::open(node, O_RDWR | O_CLOEXEC);
    if (render_fd < 0)
        throw std::runtime_error("cannot open selected GPU render node");
    gbm = gbm_create_device(render_fd);
    if (!gbm)
        throw std::runtime_error("cannot initialize GBM");
    display = platform(EGL_PLATFORM_GBM_KHR, gbm, nullptr);
    if (display == EGL_NO_DISPLAY || !eglInitialize(display, nullptr, nullptr))
        throw std::runtime_error("cannot initialize effect EGL display");
    auto *extensions = eglQueryString(display, EGL_EXTENSIONS);
    if (!extension(extensions, "EGL_EXT_image_dma_buf_import_modifiers") ||
        !extension(extensions, "EGL_KHR_surfaceless_context"))
        throw std::runtime_error("EGL DMA-BUF modifier import is unavailable");
    create_image =
        reinterpret_cast<PFNEGLCREATEIMAGEKHRPROC>(eglGetProcAddress("eglCreateImageKHR"));
    destroy_image =
        reinterpret_cast<PFNEGLDESTROYIMAGEKHRPROC>(eglGetProcAddress("eglDestroyImageKHR"));
    bind_image = reinterpret_cast<PFNGLEGLIMAGETARGETTEXTURE2DOESPROC>(
        eglGetProcAddress("glEGLImageTargetTexture2DOES"));
    if (!create_image || !destroy_image || !bind_image)
        throw std::runtime_error("EGL image functions unavailable");
    if (extension(extensions, "EGL_ANDROID_native_fence_sync")) {
        create_sync = reinterpret_cast<PFNEGLCREATESYNCKHRPROC>(eglGetProcAddress("eglCreateSyncKHR"));
        destroy_sync = reinterpret_cast<PFNEGLDESTROYSYNCKHRPROC>(eglGetProcAddress("eglDestroySyncKHR"));
        duplicate_fence = reinterpret_cast<PFNEGLDUPNATIVEFENCEFDANDROIDPROC>(
            eglGetProcAddress("eglDupNativeFenceFDANDROID"));
        // Lets this context wait for a producer's sync_file on the GPU
        // rather than blocking the processing thread until it signals.
        if (extension(extensions, "EGL_KHR_wait_sync"))
            wait_sync =
                reinterpret_cast<PFNEGLWAITSYNCKHRPROC>(eglGetProcAddress("eglWaitSyncKHR"));
    }
    if (!create_sync || !destroy_sync || !duplicate_fence || !wait_sync)
        throw std::runtime_error("RTX HDR requires native GPU fence import/export");
    const EGLenum previous_api = eglQueryAPI();
    if (!eglBindAPI(EGL_OPENGL_API))
        throw std::runtime_error("desktop OpenGL unavailable");
    const EGLint config_attributes[] = {EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT, EGL_SURFACE_TYPE,
                                        EGL_PBUFFER_BIT, EGL_NONE};
    EGLConfig config = nullptr;
    EGLint configs = 0;
    // A game saturating the GPU otherwise delays every queued conversion by
    // its scheduling timeslice. Request a high-priority context; drivers may
    // grant less (NVIDIA requires CAP_SYS_NICE), so the granted level is
    // queried and reported rather than assumed.
    priority_query = extension(extensions, "EGL_IMG_context_priority");
    const EGLint prioritized_attributes[] = {EGL_CONTEXT_MAJOR_VERSION,
                                             3,
                                             EGL_CONTEXT_MINOR_VERSION,
                                             3,
                                             EGL_CONTEXT_PRIORITY_LEVEL_IMG,
                                             EGL_CONTEXT_PRIORITY_HIGH_IMG,
                                             EGL_NONE};
    const EGLint context_attributes[] = {EGL_CONTEXT_MAJOR_VERSION, 3, EGL_CONTEXT_MINOR_VERSION, 3,
                                         EGL_NONE};
    if (eglChooseConfig(display, config_attributes, &config, 1, &configs) && configs > 0) {
        if (priority_query)
            context = eglCreateContext(display, config, EGL_NO_CONTEXT, prioritized_attributes);
        if (context == EGL_NO_CONTEXT)
            context = eglCreateContext(display, config, EGL_NO_CONTEXT, context_attributes);
    }
    eglBindAPI(previous_api);
    if (context == EGL_NO_CONTEXT)
        throw std::runtime_error("cannot create effect OpenGL context");
}

void Surface::prepare(uint32_t width, uint32_t height) {
    texture(input, width, height);
    framebuffer(input, input_fbo);
    glGenTextures(1, &source);
    glGenVertexArrays(1, &vao);
    const char *vertex = "#version 330\n"
                         "out vec2 uv; void main(){ vec2 p=vec2((gl_VertexID<<1)&2,gl_VertexID&2);"
                         "uv=p;gl_Position=vec4(p*2.0-1.0,0,1);}";
    const char *fragment = "#version 330\n"
                           "in vec2 uv; uniform sampler2D source; out vec4 color;"
                           "void main(){color=vec4(texture(source,uv).rgb,1.0);}";
    GLuint vs = shader(GL_VERTEX_SHADER, vertex), fs = 0;
    try {
        fs = shader(GL_FRAGMENT_SHADER, fragment);
    } catch (...) {
        glDeleteShader(vs);
        throw;
    }
    program = glCreateProgram();
    glAttachShader(program, vs);
    glAttachShader(program, fs);
    glLinkProgram(program);
    glDeleteShader(vs);
    glDeleteShader(fs);
    GLint linked = 0;
    glGetProgramiv(program, GL_LINK_STATUS, &linked);
    if (!linked)
        throw std::runtime_error("effect shader link failed");
    glUseProgram(program);
    glUniform1i(glGetUniformLocation(program, "source"), 0);
    glDisable(GL_DITHER);
    glDisable(GL_BLEND);
    glDisable(GL_FRAMEBUFFER_SRGB);
    gl_check("effect GL initialization failed");
}

EGLImageKHR Surface::import(const AmaterasuDmaBufFrame &frame) {
    // This backend explicitly accepts packed single-plane RGB only. No unknown
    // format, modifier, or color is reinterpreted as an SDR RGB frame.
    if (frame.plane_count != 1)
        throw std::runtime_error("effect requires a packed RGB DMA-BUF");
    const auto &plane = frame.planes[0];
    std::vector<EGLint> attributes = {EGL_WIDTH,
                                      static_cast<EGLint>(frame.width),
                                      EGL_HEIGHT,
                                      static_cast<EGLint>(frame.height),
                                      EGL_LINUX_DRM_FOURCC_EXT,
                                      static_cast<EGLint>(frame.fourcc),
                                      EGL_DMA_BUF_PLANE0_FD_EXT,
                                      plane.fd,
                                      EGL_DMA_BUF_PLANE0_OFFSET_EXT,
                                      static_cast<EGLint>(plane.offset),
                                      EGL_DMA_BUF_PLANE0_PITCH_EXT,
                                      static_cast<EGLint>(plane.pitch)};
    if (frame.modifier != DRM_FORMAT_MOD_INVALID) {
        attributes.insert(attributes.end(),
                          {EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT, static_cast<EGLint>(frame.modifier),
                           EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,
                           static_cast<EGLint>(frame.modifier >> 32)});
    }
    attributes.push_back(EGL_NONE);
    auto image =
        create_image(display, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, nullptr, attributes.data());
    if (image == EGL_NO_IMAGE_KHR)
        throw std::runtime_error("effect cannot import this GPU buffer");
    return image;
}

void Surface::wait_acquire(int fence_fd) {
    if (fence_fd < 0) return;
    const int duplicate = ::fcntl(fence_fd, F_DUPFD_CLOEXEC, 0);
    if (duplicate < 0) throw std::runtime_error("cannot duplicate input dependency");
    const EGLint attributes[] = {EGL_SYNC_NATIVE_FENCE_FD_ANDROID, duplicate, EGL_NONE};
    EGLSyncKHR sync = create_sync(display, EGL_SYNC_NATIVE_FENCE_ANDROID, attributes);
    if (sync == EGL_NO_SYNC_KHR) {
        ::close(duplicate);
        throw std::runtime_error("cannot import input dependency");
    }
    const EGLBoolean queued = wait_sync(display, sync, 0);
    destroy_sync(display, sync);
    if (queued != EGL_TRUE) throw std::runtime_error("cannot queue GPU input dependency");
}

// The caller has already queued the producer's acquire-fence wait.
void Surface::capture(const AmaterasuDmaBufFrame &frame) {
    auto image = import(frame);
    try {
        glActiveTexture(GL_TEXTURE0);
        glBindTexture(GL_TEXTURE_2D, source);
        bind_image(GL_TEXTURE_2D, image);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
        glBindFramebuffer(GL_FRAMEBUFFER, input_fbo);
        glViewport(0, 0, frame.width, frame.height);
        glUseProgram(program);
        glBindVertexArray(vao);
        glDrawArrays(GL_TRIANGLES, 0, 3);
        // The input texture is consumed either by CUDA graphics-map or by the
        // fallback GL draw in this same context. Both preserve this dependency.
        // The completion fence created after the output draw covers this read.
        gl_check("effect source sampling failed");
    } catch (...) {
        glFinish();
        destroy_image(display, image);
        throw;
    }
    destroy_image(display, image);
}

int Surface::complete() {
    EGLSyncKHR sync = create_sync(display, EGL_SYNC_NATIVE_FENCE_ANDROID, nullptr);
    if (sync == EGL_NO_SYNC_KHR) throw std::runtime_error("cannot create input read completion");
    glFlush();
    const int fd = duplicate_fence(display, sync);
    destroy_sync(display, sync);
    if (fd < 0) throw std::runtime_error("cannot export input read completion");
    return fd;
}

void Surface::close() {
    glDeleteFramebuffers(1, &input_fbo);
    glDeleteTextures(1, &input);
    glDeleteTextures(1, &source);
    glDeleteVertexArrays(1, &vao);
    glDeleteProgram(program);
}

} // namespace rtx
