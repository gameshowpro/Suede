// The GPU half of the slicer's per-pixel transfer, kept integer-exact with
// the CPU path (`Blend::rows` in slicer.rs) so switching between them never
// visibly changes a frame. Rebuild with `naga blend.frag blend.frag.spv`
// (naga is at ~/.cargo/bin/naga in WSL) and check the .spv in alongside this
// file — the target machines have no shader compiler installed.
//
// The texture and its sampler are declared separately (not as a combined
// `sampler2D` uniform) because naga's GLSL frontend (the only shader
// compiler available on the build machine — see gpu.rs) does not implement
// combined image samplers: `uniform sampler2D canvas;` fails with "Not
// implemented: variable qualifier". `gpu.rs` matches this with an immutable
// sampler baked into the descriptor set layout at binding 1, so only the
// texture (binding 0), the transfer SSBO (binding 2) and the sync pattern's
// shape SSBO (binding 3) are ever written.
#version 450

layout(set = 0, binding = 0) uniform texture2D canvas;
layout(set = 0, binding = 1) uniform sampler canvasSampler;

// `(a, b)` packed as `a << 8 | b`, row-major at this output's size — see
// `set_transfer` in gpu.rs and `pixel_transfer` in blend.rs, which both
// produce the fixed-point pair this unpacks.
layout(set = 0, binding = 2, std430) readonly buffer Transfer {
    uint table[];
};

// The `sync` test pattern's geometry: white rectangles on black, in
// output-local pixels, half-open (`x0 <= x < x1`). Laid out as `pc.groups`
// variable-length groups, each of which is a bounding box
// `(x0, y0, x1, y1)`, then `(rect_count, next_group_index, 0, 0)`, then that
// many rectangles — see `set_sync_shapes` in gpu.rs and `sync_rects` in
// pattern.rs, which builds it. The bounding box is what keeps this cheap: a
// black pixel, which is almost all of them, costs four comparisons per
// group rather than four per rectangle.
layout(set = 0, binding = 3, std430) readonly buffer Shapes {
    uvec4 shapes[];
};

layout(push_constant) uniform PushConstants {
    uint source_x;
    uint source_y;
    uint width;
    uint height;
    uint canvas_height;
    uint y_invert;
    // 0: blend the canvas. 1: draw the `sync` pattern from `shapes`.
    uint mode;
    uint groups;
} pc;

layout(location = 0) out vec4 outColor;

void main() {
    // `gl_FragCoord` is window space for whatever viewport this job's draw
    // call set, so it is already 0..width-1, 0..height-1 in output-local
    // coordinates — no per-job uniform needed for that part.
    ivec2 p = ivec2(gl_FragCoord.xy);
    uvec3 c;
    if (pc.mode == 1u) {
        uvec2 q = uvec2(p);
        uint lit = 0u;
        uint i = 0u;
        for (uint g = 0u; g < pc.groups; ++g) {
            uvec4 box = shapes[i];
            uvec4 head = shapes[i + 1u];
            if (q.x >= box.x && q.x < box.z && q.y >= box.y && q.y < box.w) {
                for (uint r = 0u; r < head.x; ++r) {
                    uvec4 s = shapes[i + 2u + r];
                    if (q.x >= s.x && q.x < s.z && q.y >= s.y && q.y < s.w) {
                        lit = 1u;
                        break;
                    }
                }
            }
            if (lit != 0u) {
                break;
            }
            i = head.y;
        }
        // Full white or full black, so the transfer below is the only thing
        // between this and the projector — which is the point: the ramps and
        // the black lift shape the counter exactly as they shape content.
        c = uvec3(lit * 255u);
    } else {
        int cy = int(pc.source_y) + p.y;
        if (pc.y_invert != 0u) {
            cy = int(pc.canvas_height) - 1 - cy;
        }
        vec4 t = texelFetch(sampler2D(canvas, canvasSampler), ivec2(int(pc.source_x) + p.x, cy), 0);
        // Same fixed-point shade as `Blend::rows`: `out = min((a*in)>>8 + b, 255)`
        // per channel, rounding the sampled float back to the byte it came from
        // first so both paths start from the identical integer.
        c = uvec3(round(t.rgb * 255.0));
    }
    uint ab = table[p.y * pc.width + p.x];
    uint a = ab >> 8u;
    uint b = ab & 0xffu;
    uvec3 o = min(((a * c) >> 8u) + b, uvec3(255u));
    outColor = vec4(vec3(o) / 255.0, 1.0);
}
