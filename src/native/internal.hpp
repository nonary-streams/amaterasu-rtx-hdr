#pragma once
// Private platform boundary. No CUDA/EGL/GL objects cross the plugin SDK.
#define GL_GLEXT_PROTOTYPES
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GL/gl.h>
#include <GL/glext.h>
#include <amaterasu_media.h>
#include <cstdint>
#include <cuda.h>
#include <cudaGL.h>
#include <gbm.h>
#include <memory>
#include <array>
#include <stdexcept>
#include <string>

namespace rtx {
// Published NvCVImage v1 C ABI, documented by NVIDIA at
// https://docs.nvidia.com/maxine/vfx/latest/API/Reference/Structures.html
struct Image {
    uint32_t width, height;
    int32_t pitch, pixel_format, component_type;
    uint8_t pixel_bytes, component_bytes, components, planar;
    uint8_t gpu_memory, colorspace, reserved[2];
    void *pixels, *delete_pointer, *delete_callback;
    uint64_t buffer_bytes;
};
static_assert(sizeof(Image) == 64);

struct Api {
    decltype(&cuInit) init;
    decltype(&cuDeviceGet) device_get;
    decltype(&cuDevicePrimaryCtxRetain) retain_context;
    decltype(&cuDevicePrimaryCtxRelease) release_context;
    decltype(&cuCtxPushCurrent) push;
    decltype(&cuCtxPopCurrent) pop;
    decltype(&cuCtxSynchronize) synchronize_context;
    decltype(&cuStreamCreate) create_stream;
    decltype(&cuStreamDestroy) destroy_stream;
    decltype(&cuStreamSynchronize) synchronize_stream;
    decltype(&cuGraphicsGLRegisterImage) register_image;
    decltype(&cuGraphicsUnregisterResource) unregister_image;
    decltype(&cuGraphicsMapResources) map;
    decltype(&cuGraphicsUnmapResources) unmap;
    decltype(&cuGraphicsSubResourceGetMappedArray) mapped_array;
    decltype(&cuMemcpy2DAsync) copy;
    int (*allocate)(Image *, unsigned, unsigned, unsigned, unsigned, unsigned, unsigned, unsigned);
    void (*deallocate)(Image *);
    int (*create_effect)(const char *, void **);
    void (*destroy_effect)(void *);
    int (*set_image)(void *, const char *, Image *);
    int (*set_u32)(void *, const char *, unsigned);
    int (*set_stream)(void *, const char *, CUstream);
    int (*load_effect)(void *);
    int (*run_effect)(void *, int);
    static std::shared_ptr<Api> load(const std::string &root);
};

inline void cuda_check(CUresult result, const char *operation) {
    if (result != CUDA_SUCCESS)
        throw std::runtime_error(std::string(operation) + ": CUDA " + std::to_string(result));
}
inline void vfx_check(int result, const char *operation) {
    if (result != 0)
        throw std::runtime_error(std::string(operation) + ": VFX " + std::to_string(result));
}
void wait_fence(int fd);

struct Surface {
    int render_fd = -1;
    gbm_device *gbm = nullptr;
    EGLDisplay display = EGL_NO_DISPLAY;
    EGLContext context = EGL_NO_CONTEXT;
    PFNEGLCREATEIMAGEKHRPROC create_image = nullptr;
    PFNEGLDESTROYIMAGEKHRPROC destroy_image = nullptr;
    PFNGLEGLIMAGETARGETTEXTURE2DOESPROC bind_image = nullptr;
    PFNEGLCREATESYNCKHRPROC create_sync = nullptr;
    PFNEGLDESTROYSYNCKHRPROC destroy_sync = nullptr;
    PFNEGLDUPNATIVEFENCEFDANDROIDPROC duplicate_fence = nullptr;
    PFNEGLWAITSYNCKHRPROC wait_sync = nullptr;
    bool priority_query = false;
    GLuint input = 0, output = 0, input_fbo = 0, output_fbo = 0;
    GLuint source = 0, program = 0, output_program = 0, fallback_program = 0, vao = 0;
    void open(CUdevice device);
    void prepare(uint32_t width, uint32_t height);
    EGLImageKHR import(const AmaterasuDmaBufFrame &frame);
    void capture(const AmaterasuDmaBufFrame &frame);
    void wait_acquire(int fence_fd);
    int complete();
    void close();
};

struct Current {
    Api &api;
    Surface &surface;
    EGLDisplay previous_display;
    EGLContext previous_context;
    EGLSurface previous_draw, previous_read;
    EGLenum previous_api;
    Current(Api &api, Surface &surface, CUcontext context);
    ~Current();
};

struct Output {
    gbm_bo *buffer = nullptr;
    EGLImageKHR image = EGL_NO_IMAGE_KHR;
    GLuint texture = 0, fbo = 0;
    AmaterasuDmaBufFrame frame{};
    void open(Surface &surface, uint32_t width, uint32_t height,
              uint32_t format = GBM_FORMAT_ABGR2101010);
    void close(Surface &surface);
};

struct Session {
    std::shared_ptr<Api> api;
    CUdevice device = 0;
    CUcontext context = nullptr;
    CUstream stream = nullptr;
    Surface surface;
    Image input{}, output{};
    void *effect = nullptr;
    CUgraphicsResource input_resource = nullptr, output_resource = nullptr;
    bool retained_context = false;
    bool enabled = true;
    uint32_t sdr_brightness = 0;
    std::array<uint32_t, 5> parameters{};
    Session(const std::string &root, int ordinal, uint32_t width, uint32_t height,
            const uint32_t *parameters);
    ~Session();
    int run(const AmaterasuDmaBufFrame &frame, Output &destination);
    void convert();
    void configure(const uint32_t *values, bool active, uint32_t brightness);
    void *create_configured_effect(const uint32_t *values);
    void close();
};
} // namespace rtx

extern "C" {
void *am_rtx_open(const char *, int, uint32_t, uint32_t, const uint32_t *, char *, size_t);
void am_rtx_close(void *);
void *am_rtx_output_open(void *, AmaterasuDmaBufFrame *, char *, size_t);
void am_rtx_output_close(void *, void *);
int am_rtx_process(void *, const AmaterasuDmaBufFrame *, void *, int *, char *, size_t);
int am_rtx_configure(void *, const uint32_t *, bool, uint32_t, char *, size_t);
int am_rtx_wait_fence(int);
int am_rtx_context_priority(void *);
}
