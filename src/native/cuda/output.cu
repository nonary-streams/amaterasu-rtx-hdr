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
