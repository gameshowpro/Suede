// The GPU half of the slicer's per-pixel transfer, kept integer-exact with
// the CPU path (`Blend::rows` in slicer.rs) so switching between them never
// visibly changes a frame. Rebuild with
// `naga --input-kind glsl --shader-stage frag blend.frag blend.frag.spv`
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
// produce the fixed-point pair this unpacks. Bits 0..7: b (0..255),
// bits 8..16: a (0..256), bits 17..31: zero. Border coverage attenuates
// both coefficients before rounding; the shader never applies it again.
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
    // When enabled, these are the inverse homography rows from output pixel
    // boundaries to the source rectangle's unit square.  The fields after
    // the original eight uints deliberately start at byte 32.
    vec4 inverse_row0;
    vec4 inverse_row1;
    vec4 inverse_row2;
    vec2 center;
    uint warp_enabled;
    uint padding;
    // Absolute canvas pixel-boundary x, y, width, height, at byte 96.
    vec4 source_rect;
    // Global adaptive-lift state. Dynamic transfer entries use these fields;
    // the legacy fixed `(a,b)` path deliberately does not observe them.
    float dynamic_lift;
    uint dynamic_maximum;
    uint dynamic_padding0;
    uint dynamic_padding1;
} pc;

layout(location = 0) out vec4 outColor;

void main() {
    // `gl_FragCoord` is window space for whatever viewport this job's draw
    // call set, so it is already 0..width-1, 0..height-1 in output-local
    // coordinates — no per-job uniform needed for that part.
    ivec2 p = ivec2(gl_FragCoord.xy);
    uvec3 c;
    uint ab = table[p.y * pc.width + p.x];
    bool dynamic = (ab & 0x80000000u) != 0u;
    uint dynamic_count = (ab >> 24u) & 0x0fu;

    // Dynamic entries are tagged with bit 31. Their bits are:
    // 0..15 r as UNORM16, 16..23 picture-border coverage as UNORM8,
    // 24..27 configured-footprint coverage count (0..8), 28..30 reserved
    // zero. Count zero is an authoritative outside-picture sentinel, so it
    // remains black even if the shared lift is nonzero.
    if (dynamic && dynamic_count == 0u) {
        outColor = vec4(0.0, 0.0, 0.0, 1.0);
        return;
    }

    if (pc.warp_enabled != 0u && ab == 0u) {
        // A zero transfer entry is outside the covered destination.  Return
        // before doing any source arithmetic so this path cannot fetch an
        // invalid canvas coordinate.
        outColor = vec4(0.0, 0.0, 0.0, 1.0);
        return;
    }

    vec2 local = vec2(p);
    vec2 unit = vec2(0.0);
    bool local_valid = true;
    if (pc.warp_enabled != 0u) {
        vec3 h = vec3(dot(pc.inverse_row0.xyz, vec3(gl_FragCoord.xy, 1.0)),
                      dot(pc.inverse_row1.xyz, vec3(gl_FragCoord.xy, 1.0)),
                      dot(pc.inverse_row2.xyz, vec3(gl_FragCoord.xy, 1.0)));
        if (any(isnan(h)) || any(isinf(h)) || abs(h.z) <= 1.0e-7 ||
            pc.center.x <= 0.0 || pc.center.x >= 1.0 ||
            pc.center.y <= 0.0 || pc.center.y >= 1.0) {
            local_valid = false;
        } else {
            vec2 s = h.xy / h.z;
            unit = vec2(s.x <= pc.center.x
                             ? s.x / (2.0 * pc.center.x)
                             : 0.5 + (s.x - pc.center.x) / (2.0 * (1.0 - pc.center.x)),
                         s.y <= pc.center.y
                             ? s.y / (2.0 * pc.center.y)
                             : 0.5 + (s.y - pc.center.y) / (2.0 * (1.0 - pc.center.y)));
            // Sync shapes stay in output-local source-pattern coordinates.
            local = unit * vec2(pc.width, pc.height);
            // A nonzero transfer entry means this destination pixel is
            // covered.  Clamp its source to a texel center, including when a
            // partially covered edge maps just outside the source rectangle.
            // Uncovered pixels took the table==0 return above.
            local = clamp(local, vec2(0.5),
                          vec2(float(pc.width) - 0.5, float(pc.height) - 0.5));
        }
    }

    if (!local_valid) {
        outColor = vec4(0.0, 0.0, 0.0, 1.0);
        return;
    } else if (pc.mode == 1u) {
        // In warp mode, keep the sync comparisons in signed floating point
        // until after bounds checks so negative coordinates cannot wrap.
        uint lit = 0u;
        uint i = 0u;
        if (pc.warp_enabled != 0u) {
            vec2 qf = local;
            for (uint g = 0u; g < pc.groups; ++g) {
                uvec4 box = shapes[i];
                uvec4 head = shapes[i + 1u];
                if (qf.x >= float(box.x) && qf.x < float(box.z) &&
                    qf.y >= float(box.y) && qf.y < float(box.w)) {
                    for (uint r = 0u; r < head.x; ++r) {
                        uvec4 s = shapes[i + 2u + r];
                        if (qf.x >= float(s.x) && qf.x < float(s.z) &&
                            qf.y >= float(s.y) && qf.y < float(s.w)) {
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
        } else {
            uvec2 q = uvec2(p);
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
        }
        // Full white or full black, so the transfer below is the only thing
        // between this and the projector — which is the point: the layout's
        // blend weight and the black lift shape the counter exactly as they
        // shape content.
        c = uvec3(lit * 255u);
    } else if (pc.warp_enabled != 0u) {
        vec2 source = pc.source_rect.xy + unit * pc.source_rect.zw;
        // The transfer table is authoritative for destination border AA.
        // Clamp covered samples to source texel centers; a source narrower
        // than one canvas pixel has a single sampling position at its middle.
        vec2 inset = min(vec2(0.5), pc.source_rect.zw * 0.5);
        source = clamp(source, pc.source_rect.xy + inset,
                       pc.source_rect.xy + max(inset, pc.source_rect.zw - inset));
        if (pc.y_invert != 0u) {
            source.y = float(pc.canvas_height) - source.y;
        }
        vec2 canvas_size = vec2(textureSize(canvas, 0));
        if (any(isnan(source)) || any(isinf(source)) ||
            source.x < 0.5 || source.x > canvas_size.x - 0.5 ||
            source.y < 0.5 || source.y > canvas_size.y - 0.5) {
            outColor = vec4(0.0, 0.0, 0.0, 1.0);
            return;
        } else {
            vec4 t = textureLod(sampler2D(canvas, canvasSampler),
                                source / canvas_size, 0.0);
            c = uvec3(round(t.rgb * 255.0));
        }
    } else {
        // Validate unsigned origins before addition or y inversion. An
        // out-of-canvas pixel is opaque black, including with nonzero lift.
        uvec2 canvas_size = uvec2(textureSize(canvas, 0));
        if (pc.source_x >= canvas_size.x || pc.source_y >= canvas_size.y ||
            uint(p.x) >= canvas_size.x - pc.source_x ||
            uint(p.y) >= canvas_size.y - pc.source_y) {
            outColor = vec4(0.0, 0.0, 0.0, 1.0);
            return;
        }
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
    if (dynamic) {
        float r = float(ab & 0xffffu) / 65535.0;
        float e = float((ab >> 16u) & 0xffu) / 255.0;
        float n = float(dynamic_count);
        float shortfall = max(float(pc.dynamic_maximum) - n, 0.0) / n;
        float lift = clamp(pc.dynamic_lift * shortfall, 0.0, 1.0);
        vec3 signal = vec3(c) / 255.0;
        // `floor(x + .5)` fixes the rounding rule across drivers. Saturate
        // before converting to uint so a white at any lift remains white.
        uvec3 o = uvec3(floor(min(255.0 * e * ((1.0 - lift) * r * signal + lift),
                                 vec3(255.0)) + vec3(0.5)));
        outColor = vec4(vec3(o) / 255.0, 1.0);
    } else {
        // Keep this legacy path byte-for-byte unchanged: fixed lift remains
        // the precomputed `(a,b)` arithmetic used by the CPU renderer.
        uint a = ab >> 8u;
        uint b = ab & 0xffu;
        uvec3 o = min(((a * c) >> 8u) + b, uvec3(255u));
        outColor = vec4(vec3(o) / 255.0, 1.0);
    }
}
