// RTX HDR output kernels for the CUDA resource handoff.
//
// These reproduce the GL output shaders in surface.cpp exactly (same formulas,
// same clamp and the same round-to-nearest UNORM10 store), writing packed
// X2B10G10R10 words (red in bits 0-9, alpha bits set) into a CUDA slot.
// Regenerate output.ptx with scripts/generate-cuda-ptx.sh after editing.

__device__ static float3 decode_pq(float3 v) {
    float3 result;
    const float *in = &v.x;
    float *out = &result.x;
    for (int channel = 0; channel < 3; ++channel) {
        const float p = powf(in[channel], 32.0f / 2523.0f);
        out[channel] = 10000.0f * powf(fmaxf(p - 0.8359375f, 0.0f) / (18.8515625f - 18.6875f * p),
                                       16384.0f / 2610.0f);
    }
    return result;
}

__device__ static float encode_pq_channel(float nits) {
    const float p = powf(fminf(fmaxf(nits / 10000.0f, 0.0f), 1.0f), 2610.0f / 16384.0f);
    return powf((0.8359375f + 18.8515625f * p) / (1.0f + 18.6875f * p), 2523.0f / 32.0f);
}

__device__ static unsigned int unorm10(float value) {
    return __float2uint_rn(fminf(fmaxf(value, 0.0f), 1.0f) * 1023.0f);
}

__device__ static unsigned int pack(float red, float green, float blue) {
    return unorm10(red) | (unorm10(green) << 10) | (unorm10(blue) << 20) | (3u << 30);
}

// Model output (packed PQ words) to the stream's PQ output, applying the
// peak-luminance expansion above the model's 1000-nit ceiling.
extern "C" __global__ void am_rtx_pq_output(const unsigned char *source, unsigned int source_pitch,
                                            unsigned char *target, unsigned int target_pitch,
                                            int width, int height, float peak, float gray) {
    const int x = blockIdx.x * blockDim.x + threadIdx.x;
    const int y = blockIdx.y * blockDim.y + threadIdx.y;
    if (x >= width || y >= height)
        return;
    const unsigned int word =
        reinterpret_cast<const unsigned int *>(source + (size_t)y * source_pitch)[x];
    float3 code = make_float3((float)(word & 1023u) / 1023.0f,
                              (float)((word >> 10) & 1023u) / 1023.0f,
                              (float)((word >> 20) & 1023u) / 1023.0f);
    if (peak > 1000.0f) {
        const float native_gray = fminf(fmaxf(floorf(gray * 1000.0f / peak + 0.5f), 10.0f), 100.0f);
        float3 light = decode_pq(code);
        float *channel = &light.x;
        for (int index = 0; index < 3; ++index) {
            if (gray * 1000.0f / peak < 10.0f) {
                const float low = channel[index] * (gray / native_gray);
                const float high =
                    gray + (channel[index] - native_gray) * (peak - gray) / (1000.0f - native_gray);
                channel[index] = channel[index] > native_gray ? high : low;
            } else {
                channel[index] *= peak / 1000.0f;
            }
        }
        code = make_float3(encode_pq_channel(light.x), encode_pq_channel(light.y),
                           encode_pq_channel(light.z));
    }
    const float ceiling = 846.0f / 1023.0f;
    reinterpret_cast<unsigned int *>(target + (size_t)y * target_pitch)[x] =
        pack(fminf(code.x, ceiling), fminf(code.y, ceiling), fminf(code.z, ceiling));
}

// Conversion disabled: sRGB input shown at a fixed SDR white in the PQ stream.
extern "C" __global__ void am_rtx_sdr_output(const unsigned char *source, unsigned int source_pitch,
                                             unsigned char *target, unsigned int target_pitch,
                                             int width, int height, float white_nits) {
    const int x = blockIdx.x * blockDim.x + threadIdx.x;
    const int y = blockIdx.y * blockDim.y + threadIdx.y;
    if (x >= width || y >= height)
        return;
    const uchar4 texel = reinterpret_cast<const uchar4 *>(source + (size_t)y * source_pitch)[x];
    float c[3] = {texel.x / 255.0f, texel.y / 255.0f, texel.z / 255.0f};
    float linear[3];
    for (int index = 0; index < 3; ++index)
        linear[index] = c[index] > 0.04045f ? powf((c[index] + 0.055f) / 1.055f, 2.4f)
                                            : c[index] / 12.92f;
    // Linear BT.709 to BT.2020 primaries (the GLSL matrix, column major).
    const float wide[3] = {
        0.627404f * linear[0] + 0.329283f * linear[1] + 0.043313f * linear[2],
        0.069097f * linear[0] + 0.919540f * linear[1] + 0.011362f * linear[2],
        0.016391f * linear[0] + 0.088013f * linear[1] + 0.895595f * linear[2],
    };
    float code[3];
    for (int index = 0; index < 3; ++index) {
        const float p = powf(fmaxf(wide[index], 0.0f) * (white_nits / 10000.0f), 2610.0f / 16384.0f);
        code[index] = powf((0.8359375f + 18.8515625f * p) / (1.0f + 18.6875f * p), 2523.0f / 32.0f);
    }
    reinterpret_cast<unsigned int *>(target + (size_t)y * target_pitch)[x] =
        pack(code[0], code[1], code[2]);
}

// Explicit CPU-source import. The staging allocation is uploaded in source
// layout; this GPU kernel converts that negotiated layout to the VFX RGBA8 input.
__device__ static float input_srgb(float encoded, unsigned int transfer) {
    encoded = fminf(fmaxf(encoded, 0.0f), 1.0f);
    if (transfer == 1u) return encoded; // sRGB is already the model's input.
    const float linear = encoded < 0.081f ? encoded / 4.5f
        : powf((encoded + 0.099f) / 1.099f, 1.0f / 0.45f);
    return linear < 0.0031308f ? 12.92f * linear : 1.055f * powf(linear, 1.0f / 2.4f) - 0.055f;
}
extern "C" __global__ void am_rtx_cpu_input(const unsigned char *source, unsigned int source_pitch,
    unsigned char *target, unsigned int target_pitch, int width, int height,
    unsigned int format, unsigned int transfer, unsigned int range) {
    const int x = blockIdx.x * blockDim.x + threadIdx.x;
    const int y = blockIdx.y * blockDim.y + threadIdx.y;
    if (x >= width || y >= height) return;
    float red, green, blue;
    if (format == 1u) { // BGRA8
        const uchar4 value = reinterpret_cast<const uchar4 *>(source + (size_t)y * source_pitch)[x];
        red = value.z / 255.0f; green = value.y / 255.0f; blue = value.x / 255.0f;
    } else {
        const unsigned char *chroma = source + (size_t)(height + y / 2) * source_pitch;
        float luma, cb, cr;
        if (format == 3u) { // P010: high ten bits of little-endian words.
            luma = (float)(reinterpret_cast<const unsigned short *>(source + (size_t)y * source_pitch)[x] >> 6);
            cb = (float)(reinterpret_cast<const unsigned short *>(chroma)[x & ~1] >> 6);
            cr = (float)(reinterpret_cast<const unsigned short *>(chroma)[(x & ~1) + 1] >> 6);
            luma = range == 2u ? (luma - 64.0f) / 876.0f : luma / 1023.0f;
            cb = (cb - 512.0f) / (range == 2u ? 896.0f : 1023.0f);
            cr = (cr - 512.0f) / (range == 2u ? 896.0f : 1023.0f);
        } else { // NV12
            luma = source[(size_t)y * source_pitch + x];
            cb = chroma[x & ~1]; cr = chroma[(x & ~1) + 1];
            luma = range == 2u ? (luma - 16.0f) / 219.0f : luma / 255.0f;
            cb = (cb - 128.0f) / (range == 2u ? 224.0f : 255.0f);
            cr = (cr - 128.0f) / (range == 2u ? 224.0f : 255.0f);
        }
        red = luma + 1.5748f * cr;
        green = luma - 0.187324f * cb - 0.468124f * cr;
        blue = luma + 1.8556f * cb;
    }
    uchar4 value;
    value.x = (unsigned char)__float2uint_rn(input_srgb(red, transfer) * 255.0f);
    value.y = (unsigned char)__float2uint_rn(input_srgb(green, transfer) * 255.0f);
    value.z = (unsigned char)__float2uint_rn(input_srgb(blue, transfer) * 255.0f);
    value.w = 255;
    reinterpret_cast<uchar4 *>(target + (size_t)y * target_pitch)[x] = value;
}
