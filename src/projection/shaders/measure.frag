// Source-space luminance measurement pass. It samples each point of a small,
// regular nearest-texel grid over the logical canvas exactly once; ramps,
// output overlap, border coverage, and black lift are intentionally absent.
#version 450

layout(set = 0, binding = 0) uniform texture2D canvas;
layout(set = 0, binding = 1) uniform sampler canvasSampler;

layout(push_constant) uniform PushConstants {
    uint source_width;
    uint source_height;
    uint grid_width;
    uint grid_height;
    uint y_invert;
} pc;

layout(location = 0) out vec4 outColor;

void main() {
    uvec2 p = uvec2(gl_FragCoord.xy);
    // The centers of equal source-domain cells include both ends of the
    // logical canvas without sampling allocation padding.
    // Bounds are <=32768 and the grid <=256, so this exact integer form
    // cannot overflow u32 and matches adaptive::SampleGrid::pixel.
    uvec2 q = uvec2(((2u * p.x + 1u) * pc.source_width) / (2u * pc.grid_width),
                    ((2u * p.y + 1u) * pc.source_height) / (2u * pc.grid_height));
    q = min(q, uvec2(pc.source_width - 1u, pc.source_height - 1u));
    if (pc.y_invert != 0u) {
        q.y = uint(textureSize(canvas, 0).y) - 1u - q.y;
    }
    outColor = texelFetch(sampler2D(canvas, canvasSampler), ivec2(q), 0);
}
