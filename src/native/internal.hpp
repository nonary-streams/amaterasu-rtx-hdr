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
    decltype(&cuEventCreate) create_event;
    decltype(&cuEventDestroy) destroy_event;
    decltype(&cuEventRecord) record_event;
    decltype(&cuEventQuery) query_event;
    decltype(&cuEventElapsedTime) elapsed_time;
    decltype(&cuDeviceGetUuid) device_uuid;
    decltype(&cuModuleLoadData) load_module;
    decltype(&cuModuleUnload) unload_module;
    decltype(&cuModuleGetFunction) module_function;
    decltype(&cuLaunchKernel) launch;
    decltype(&cuMemAllocPitch) allocate_pitch;
    decltype(&cuMemFree) free_memory;
    decltype(&cuStreamWaitEvent) wait_event;
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

/* GPU-clock stage times summed since the last take. Mirrored in linux.rs. */
struct TimingTotals {
    uint64_t samples, slow_frames;
    uint64_t queue_us, queue_max_us;   /* submit until the GPU starts sampling */
    uint64_t sample_us, sample_max_us; /* input sampling draw */
    uint64_t model_us, model_max_us;   /* sampled until GL resumes after CUDA */
    uint64_t model_run_us, model_run_max_us; /* CUDA execution of the model */
    uint64_t output_us, output_max_us; /* PQ output draw */
    uint64_t total_us, total_max_us;   /* submit until the output is written */
};
static_assert(sizeof(TimingTotals) == 14 * sizeof(uint64_t));

/* Timestamps for a few frames in flight. Results are read only once ready,
 * so measuring never makes the processing thread wait for the GPU. */
class Timing {
  public:
    void open(Api &api);
    void close(Api &api);
    bool begin(Api &api);
    void started();
    void sampled();
    void model_queued(Api &api, CUstream stream, bool before);
    void resumed();
    void finished();
    TimingTotals take(Api &api);

  private:
    struct Frame {
        GLuint queries[4]{};
        CUevent events[2]{};
        GLint64 submitted = 0;
        bool pending = false, modeled = false;
    };
    void reap(Api &api);
    std::array<Frame, 4> frames{};
    Frame *current = nullptr;
    size_t next = 0;
    bool available = false;
    TimingTotals totals{};
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

/* One CUDA output slot: pitch-linear X2B10G10R10 memory in the primary
 * context and the event recorded after each write into it. */
struct CudaOutput {
    CUdeviceptr pixels = 0;
    size_t pitch = 0;
    CUevent ready = nullptr;
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
    Timing timing;
    CUmodule kernels = nullptr;
    CUfunction pq_output = nullptr, sdr_output = nullptr;
    bool enabled = true;
    uint32_t sdr_brightness = 0;
    std::array<uint32_t, 5> parameters{};
    Session(const std::string &root, int ordinal, uint32_t width, uint32_t height,
            const uint32_t *parameters);
    ~Session();
    int run(const AmaterasuDmaBufFrame &frame, Output &destination);
    void convert();
    void copy_input();
    void load_kernels();
    void write_output(CudaOutput &destination);
    void convert_to_cuda(CudaOutput &destination);
    int run_cuda(const AmaterasuDmaBufFrame &frame, CudaOutput &destination);
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
void am_rtx_take_timing(void *, rtx::TimingTotals *);
int am_rtx_device_uuid(const char *, int, uint8_t *, char *, size_t);
void *am_rtx_cuda_output_open(void *, uint64_t *, uint32_t *, void **, char *, size_t);
void am_rtx_cuda_output_close(void *, void *);
int am_rtx_process_cuda(void *, const AmaterasuDmaBufFrame *, void *, int *, char *, size_t);
}
