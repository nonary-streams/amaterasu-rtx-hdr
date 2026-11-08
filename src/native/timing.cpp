// GPU-clock stage timing for live diagnosis of conversion latency.
//
// Every query and event belongs to the session's GL and CUDA contexts, which
// the caller makes current. A frame is measured only when a free record is
// available, and results are read only after the GPU reports them ready.
#include "internal.hpp"
#include <algorithm>

namespace rtx {
namespace {
// One and a half frame intervals at 120 Hz: such a frame delays its successor.
constexpr uint64_t slow_frame_us = 12'500;

void add(uint64_t &sum, uint64_t &maximum, uint64_t value) {
    sum += value;
    maximum = std::max(maximum, value);
}

uint64_t micros(GLuint64 later, GLuint64 earlier) {
    return later > earlier ? (later - earlier) / 1000 : 0;
}
} // namespace

void Timing::open(Api &api) {
    for (auto &frame : frames) {
        glGenQueries(4, frame.queries);
        // Event timing works across a stream even with the default flags.
        for (auto &event : frame.events)
            if (api.create_event(&event, CU_EVENT_DEFAULT) != CUDA_SUCCESS)
                event = nullptr;
    }
    available = glGetError() == GL_NO_ERROR;
}

void Timing::close(Api &api) {
    for (auto &frame : frames) {
        glDeleteQueries(4, frame.queries);
        for (auto &event : frame.events)
            if (event)
                api.destroy_event(event);
        frame = {};
    }
    available = false;
    current = nullptr;
}

bool Timing::begin(Api &api) {
    current = nullptr;
    if (!available)
        return false;
    reap(api);
    Frame &frame = frames[next];
    if (frame.pending)
        return false; // The GPU is far behind; skip rather than wait.
    next = (next + 1) % frames.size();
    frame.modeled = false;
    glGetInteger64v(GL_TIMESTAMP, &frame.submitted);
    current = &frame;
    return true;
}

void Timing::started() {
    if (current)
        glQueryCounter(current->queries[0], GL_TIMESTAMP);
}

void Timing::sampled() {
    if (current)
        glQueryCounter(current->queries[1], GL_TIMESTAMP);
}

void Timing::model_queued(Api &api, CUstream stream, bool before) {
    if (!current || !current->events[0] || !current->events[1])
        return;
    if (api.record_event(current->events[before ? 0 : 1], stream) != CUDA_SUCCESS)
        current->modeled = false;
    else if (!before)
        current->modeled = true;
}

void Timing::resumed() {
    if (current)
        glQueryCounter(current->queries[2], GL_TIMESTAMP);
}

void Timing::finished() {
    if (!current)
        return;
    glQueryCounter(current->queries[3], GL_TIMESTAMP);
    current->pending = true;
    current = nullptr;
}

void Timing::reap(Api &api) {
    for (auto &frame : frames) {
        if (!frame.pending)
            continue;
        GLint ready = 0;
        glGetQueryObjectiv(frame.queries[3], GL_QUERY_RESULT_AVAILABLE, &ready);
        if (!ready)
            continue;
        if (frame.modeled && api.query_event(frame.events[1]) == CUDA_ERROR_NOT_READY)
            continue;
        GLuint64 stamps[4] = {};
        for (int index = 0; index < 4; ++index)
            glGetQueryObjectui64v(frame.queries[index], GL_QUERY_RESULT, &stamps[index]);
        frame.pending = false;
        const auto submitted = static_cast<GLuint64>(std::max<GLint64>(frame.submitted, 0));
        totals.samples += 1;
        add(totals.queue_us, totals.queue_max_us, micros(stamps[0], submitted));
        add(totals.sample_us, totals.sample_max_us, micros(stamps[1], stamps[0]));
        add(totals.model_us, totals.model_max_us, micros(stamps[2], stamps[1]));
        add(totals.output_us, totals.output_max_us, micros(stamps[3], stamps[2]));
        const uint64_t total = micros(stamps[3], submitted);
        add(totals.total_us, totals.total_max_us, total);
        totals.slow_frames += total > slow_frame_us;
        float milliseconds = 0;
        if (frame.modeled &&
            api.elapsed_time(&milliseconds, frame.events[0], frame.events[1]) == CUDA_SUCCESS)
            add(totals.model_run_us, totals.model_run_max_us,
                static_cast<uint64_t>(milliseconds * 1000.0f));
    }
}

TimingTotals Timing::take(Api &api) {
    if (available)
        reap(api);
    const TimingTotals window = totals;
    totals = {};
    return window;
}
} // namespace rtx
