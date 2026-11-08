#include "internal.hpp"
#include <algorithm>
#include <cstring>
#include <dlfcn.h>
#include <filesystem>
#include <mutex>
#include <vector>

namespace rtx {
namespace {
template <class T> void symbol(T &target, void *library, const char *name) {
    target = reinterpret_cast<T>(dlsym(library, name));
    if (!target)
        throw std::runtime_error(std::string("missing runtime symbol: ") + name);
}
void *open_library(const std::filesystem::path &path) {
    // NVIDIA registers process-wide runtime callbacks. NODELETE keeps vendor
    // code mapped after plugin unload; effects, streams and images still retire.
    void *library = dlopen(path.c_str(), RTLD_NOW | RTLD_GLOBAL | RTLD_NODELETE);
    if (!library)
        throw std::runtime_error(std::string("runtime load failed: ") + dlerror());
    return library;
}
} // namespace

std::shared_ptr<Api> Api::load(const std::string &root) {
    static std::mutex mutex;
    static std::string loaded_root;
    static std::shared_ptr<Api> loaded;
    std::lock_guard<std::mutex> lock(mutex);
    const auto canonical = std::filesystem::canonical(root);
    if (loaded) {
        if (loaded_root != canonical.string())
            throw std::runtime_error("a different VFX runtime is already loaded");
        return loaded;
    }
    auto api = std::make_shared<Api>();
    void *driver = open_library("libcuda.so.1");
#define CUDA_MEMBER(member, name) symbol(api->member, driver, name)
    CUDA_MEMBER(init, "cuInit");
    CUDA_MEMBER(device_get, "cuDeviceGet");
    CUDA_MEMBER(retain_context, "cuDevicePrimaryCtxRetain");
    CUDA_MEMBER(release_context, "cuDevicePrimaryCtxRelease_v2");
    CUDA_MEMBER(push, "cuCtxPushCurrent_v2");
    CUDA_MEMBER(pop, "cuCtxPopCurrent_v2");
    CUDA_MEMBER(synchronize_context, "cuCtxSynchronize");
    CUDA_MEMBER(create_stream, "cuStreamCreate");
    CUDA_MEMBER(destroy_stream, "cuStreamDestroy_v2");
    CUDA_MEMBER(synchronize_stream, "cuStreamSynchronize");
    CUDA_MEMBER(register_image, "cuGraphicsGLRegisterImage");
    CUDA_MEMBER(unregister_image, "cuGraphicsUnregisterResource");
    CUDA_MEMBER(map, "cuGraphicsMapResources");
    CUDA_MEMBER(unmap, "cuGraphicsUnmapResources");
    CUDA_MEMBER(mapped_array, "cuGraphicsSubResourceGetMappedArray");
    CUDA_MEMBER(copy, "cuMemcpy2DAsync_v2");
    CUDA_MEMBER(create_event, "cuEventCreate");
    CUDA_MEMBER(destroy_event, "cuEventDestroy_v2");
    CUDA_MEMBER(record_event, "cuEventRecord");
    CUDA_MEMBER(query_event, "cuEventQuery");
    CUDA_MEMBER(elapsed_time, "cuEventElapsedTime_v2");
    CUDA_MEMBER(device_uuid, "cuDeviceGetUuid_v2");
    CUDA_MEMBER(load_module, "cuModuleLoadData");
    CUDA_MEMBER(unload_module, "cuModuleUnload");
    CUDA_MEMBER(module_function, "cuModuleGetFunction");
    CUDA_MEMBER(launch, "cuLaunchKernel");
    CUDA_MEMBER(allocate_pitch, "cuMemAllocPitch_v2");
    CUDA_MEMBER(free_memory, "cuMemFree_v2");
    CUDA_MEMBER(wait_event, "cuStreamWaitEvent");
#undef CUDA_MEMBER
    // Keep NVIDIA's wheel layout. Resolve its private shared-library dependency
    // graph in bounded passes; do not change process environment/search paths.
    std::vector<std::filesystem::path> pending;
    for (const auto &directory : {canonical / "nvidia_vfx.libs", canonical / "nvvfx/libs"}) {
        for (const auto &entry : std::filesystem::directory_iterator(directory)) {
            const auto name = entry.path().filename().string();
            // Effect libraries depend on VideoFXLocal and the NGX dispatcher.
            // Skip every effect during dependency preloading, including unrelated
            // effects shipped in the wheel; load TrueHDR explicitly afterwards.
            const bool dispatcher_or_feature =
                name == "libnvngxruntime.so" || name == "libVideoFXLocal.so" ||
                name == "libVideoFX.so" || name.find("libnvVFX") == 0 ||
                name.find("libnvidia-ngx-") == 0;
            if (entry.is_regular_file() && name.find(".so") != std::string::npos &&
                !dispatcher_or_feature)
                pending.push_back(entry.path());
        }
    }
    if (pending.empty() || pending.size() > 32)
        throw std::runtime_error("unexpected VFX runtime layout");
    std::sort(pending.begin(), pending.end());
    while (!pending.empty()) {
        size_t previous = pending.size();
        for (auto item = pending.begin(); item != pending.end();) {
            if (dlopen(item->c_str(), RTLD_NOW | RTLD_GLOBAL | RTLD_NODELETE))
                item = pending.erase(item);
            else
                ++item;
        }
        if (pending.size() == previous)
            throw std::runtime_error("unresolved VFX dependency: " +
                                     pending.front().filename().string());
    }
    for (const char *name : {"libnvngxruntime.so", "libVideoFXLocal.so", "libVideoFX.so",
                             "libnvVFXTrueHDR.so", "libnvidia-ngx-truehdr.so.1.2.3"})
        open_library(canonical / "nvvfx/libs" / name);
    void *cv = open_library(canonical / "nvvfx/libs/libNVCVImage.so");
    void *vfx = open_library(canonical / "nvvfx/libs/libVideoFX.so");
    symbol(api->allocate, cv, "NvCVImage_Alloc");
    symbol(api->deallocate, cv, "NvCVImage_Dealloc");
    symbol(api->create_effect, vfx, "NvVFX_CreateEffect");
    symbol(api->destroy_effect, vfx, "NvVFX_DestroyEffect");
    symbol(api->set_image, vfx, "NvVFX_SetImage");
    symbol(api->set_u32, vfx, "NvVFX_SetU32");
    symbol(api->set_stream, vfx, "NvVFX_SetCudaStream");
    symbol(api->load_effect, vfx, "NvVFX_Load");
    symbol(api->run_effect, vfx, "NvVFX_Run");
    loaded_root = canonical.string();
    loaded = api;
    return api;
}

Current::Current(Api &api_, Surface &surface_, CUcontext context) : api(api_), surface(surface_) {
    previous_display = eglGetCurrentDisplay();
    previous_context = eglGetCurrentContext();
    previous_draw = eglGetCurrentSurface(EGL_DRAW);
    previous_read = eglGetCurrentSurface(EGL_READ);
    previous_api = eglQueryAPI();
    cuda_check(api.push(context), "push CUDA context");
    if (!eglBindAPI(EGL_OPENGL_API) ||
        !eglMakeCurrent(surface.display, EGL_NO_SURFACE, EGL_NO_SURFACE, surface.context)) {
        CUcontext popped;
        api.pop(&popped);
        eglBindAPI(previous_api);
        throw std::runtime_error("cannot select effect EGL context");
    }
}
Current::~Current() {
    const bool restored =
        previous_display != EGL_NO_DISPLAY
            ? eglMakeCurrent(previous_display, previous_draw, previous_read, previous_context)
            : eglMakeCurrent(surface.display, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
    CUcontext popped;
    if (!restored || !eglBindAPI(previous_api) || api.pop(&popped) != CUDA_SUCCESS)
        std::abort();
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
extern "C" void *am_rtx_open(const char *root, int device, uint32_t width, uint32_t height,
                             const uint32_t *parameters, char *error, size_t capacity) {
    try {
        return new rtx::Session(root, device, width, height, parameters);
    } catch (const std::exception &failure) {
        report(error, capacity, failure);
        return nullptr;
    }
}
extern "C" void am_rtx_close(void *context) { delete static_cast<rtx::Session *>(context); }
extern "C" int am_rtx_finish(void *context) {
    auto &session = *static_cast<rtx::Session *>(context);
    try {
        rtx::Current current(*session.api, session.surface, session.context);
        rtx::cuda_check(session.api->synchronize_stream(session.stream), "retire input reads");
        glFinish();
        return glGetError() == GL_NO_ERROR ? 0 : -1;
    } catch (...) { return -1; }
}
extern "C" int am_rtx_configure(void *context, const uint32_t *parameters, bool enabled,
                                 uint32_t brightness, char *error, size_t capacity) {
    auto &session = *static_cast<rtx::Session *>(context);
    try {
        rtx::Current current(*session.api, session.surface, session.context);
        session.configure(parameters, enabled, brightness);
        return 0;
    } catch (const std::exception &failure) {
        report(error, capacity, failure);
        return -1;
    }
}
extern "C" void am_rtx_take_timing(void *context, rtx::TimingTotals *totals) {
    auto &session = *static_cast<rtx::Session *>(context);
    *totals = {};
    try {
        rtx::Current current(*session.api, session.surface, session.context);
        *totals = session.timing.take(*session.api);
    } catch (...) {
        // Diagnostics never fail the stream; report an empty window.
    }
}
extern "C" int am_rtx_context_priority(void *context) {
    auto &surface = static_cast<rtx::Session *>(context)->surface;
    EGLint priority = EGL_CONTEXT_PRIORITY_MEDIUM_IMG;
    if (!surface.priority_query ||
        !eglQueryContext(surface.display, surface.context, EGL_CONTEXT_PRIORITY_LEVEL_IMG,
                         &priority))
        return 0;
    return priority;
}
