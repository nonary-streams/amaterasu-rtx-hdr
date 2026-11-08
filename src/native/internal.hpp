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
    GLuint input = 0, input_fbo = 0;
    GLuint source = 0, program = 0, vao = 0;
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
    uint64_t samples;
    uint64_t gl_queue_us, gl_queue_max_us;
    uint64_t gl_sample_us, gl_sample_max_us;
    uint64_t cuda_pipeline_us, cuda_pipeline_max_us;
};
static_assert(sizeof(TimingTotals) == 7 * sizeof(uint64_t));

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
    void finished();
    TimingTotals take(Api &api);

  private:
    struct Frame {
        GLuint queries[2]{};
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

/* One CUDA output slot: pitch-linear X2B10G10R10 memory in the primary
 * context and the event recorded after each write into it. */
struct CudaOutput {
    CUdeviceptr pixels = 0;
    size_t pitch = 0;
    CUevent ready = nullptr;
};

void copy_graphics_input(Api &, CUstream, CUgraphicsResource, Image &);

struct Session {
    std::shared_ptr<Api> api;
    CUdevice device = 0;
    CUcontext context = nullptr;
    CUstream stream = nullptr;
    Surface surface;
    Image input{}, output{};
    void *effect = nullptr;
    CUgraphicsResource input_resource = nullptr;
    bool retained_context = false;
    Timing timing;
    CUmodule kernels = nullptr;
    CUfunction pq_output = nullptr, sdr_output = nullptr, cpu_input = nullptr;
    CUdeviceptr upload = 0;
    size_t upload_pitch = 0, upload_rows = 0;
    void upload_cpu(const AmaterasuCpuFrame &frame, const AmaterasuFrameColorMetadata &color);
    void run_cpu_cuda(const AmaterasuCpuFrame &frame, const AmaterasuFrameColorMetadata &color, CudaOutput &destination);
    bool enabled = true;
    uint32_t sdr_brightness = 0;
    std::array<uint32_t, 5> parameters{};
    Session(const std::string &root, int ordinal, uint32_t width, uint32_t height,
            const uint32_t *parameters);
    ~Session();
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
int am_rtx_finish(void *);
int am_rtx_configure(void *, const uint32_t *, bool, uint32_t, char *, size_t);
int am_rtx_context_priority(void *);
void am_rtx_take_timing(void *, rtx::TimingTotals *);
int am_rtx_device_uuid(const char *, int, uint8_t *, char *, size_t);
void *am_rtx_cuda_output_open(void *, uint64_t *, uint32_t *, void **, char *, size_t);
void am_rtx_cuda_output_close(void *, void *);
int am_rtx_process_cuda(void *, const AmaterasuDmaBufFrame *, void *, int *, char *, size_t);
}
