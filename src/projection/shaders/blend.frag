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
    // Bits 0..3: the output transform the exact copy (`warp_enabled == 0`)
    // permutes texels by, coded as `transform_flags` in gpu.rs does; the
    // warp path ignores them, its inverse rows already compose the
    // transform. Bits 4..7: the warp path's resampling kernel (`FILTER_*`
    // below), and bits 8..11 / 12..15 the area kernel's box width / height
    // minus one, as `Kernel::flags` in resample.rs codes them; the exact
    // copy ignores those. Every other bit is zero.
    uint flags;
    // Absolute canvas pixel-boundary x, y, width, height, at byte 96.
    vec4 source_rect;
    // Global adaptive-lift state. Dynamic transfer entries use these fields;
    // the legacy fixed `(a,b)` path deliberately does not observe them.
    float dynamic_lift;
    uint dynamic_maximum;
    // The logical (as-viewed) raster size, at byte 120. Equal to
    // `width`/`height` unless the slicer owns an axis-swapping output
    // transform, in which case the target is the physical raster and the
    // sync shapes (generated on the logical raster) are hit-tested here.
    uint logical_width;
    uint logical_height;
} pc;

layout(location = 0) out vec4 outColor;

// The exact copy's inverse transform permutation, indexed by the transform
// code in `pc.flags` bits 0..3: 0 normal, 1 90, 2 180, 3 270, 4 flipped,
// 5 flipped-90, 6 flipped-180, 7 flipped-270. Bit `t` of each mask says
// whether code `t` first swaps the physical axes, then mirrors the logical
// x, then mirrors the logical y. Codes 8..15 have no bits and copy as
// normal. `Transform::logical_index` in desired.rs is the reference; the
// `exact_permutation_masks_in_both_shaders_match_logical_index` test in
// gpu.rs reads these three lines and holds them to it.
const uint EXACT_SWAP_AXES = 0xAAu;
const uint EXACT_MIRROR_X = 0x96u;
const uint EXACT_MIRROR_Y = 0xCCu;

// The logical (as-viewed) pixel index of physical target pixel `p` on the
// exact path. `pc.width`/`pc.height` are the physical raster and
// `pc.logical_width`/`pc.logical_height` the logical one, so a mirrored
// index stays in range. Normal (code 0) returns `p` unchanged.
uvec2 exact_logical_index(uvec2 p) {
    uint t = pc.flags & 0xfu;
    uvec2 q = ((EXACT_SWAP_AXES >> t) & 1u) != 0u ? p.yx : p;
    if (((EXACT_MIRROR_X >> t) & 1u) != 0u) {
        q.x = pc.logical_width - 1u - q.x;
    }
    if (((EXACT_MIRROR_Y >> t) & 1u) != 0u) {
        q.y = pc.logical_height - 1u - q.y;
    }
    return q;
}

// The warp path's resampling kernels, coded in `pc.flags` bits 4..7 as
// `Kernel::flags` in resample.rs does (its module doc defines each one, and
// `shader_filter_codes_match_the_kernel_flags` holds these lines to it).
// Bilinear is code 0, so a block without a kernel samples as it always has.
const uint FILTER_BILINEAR = 0u;
const uint FILTER_NEAREST = 1u;
const uint FILTER_BICUBIC = 2u;
const uint FILTER_LANCZOS = 3u;
const uint FILTER_AREA = 4u;

const float PI = 3.14159265358979;

// The byte value of the canvas texel at unflipped canvas index `i`, with `i`
// first clamped to `lo..hi`: the texels the source rectangle touches, so no
// kernel tap reads past the slice's edge (bilinear, through the LINEAR
// sampler, is the one kernel that does not come through here). Capture y
// inversion happens here, per tap, so the kernels themselves work in canvas
// orientation and never depend on it.
uvec3 filter_texel(ivec2 i, ivec2 lo, ivec2 hi) {
    ivec2 last = ivec2(textureSize(canvas, 0)) - ivec2(1);
    i = clamp(i, lo, hi);
    if (pc.y_invert != 0u) {
        i.y = int(pc.canvas_height) - 1 - i.y;
    }
    i = clamp(i, ivec2(0), last);
    return uvec3(round(texelFetch(sampler2D(canvas, canvasSampler), i, 0).rgb * 255.0));
}

// The separable kernels' weight at distance `x` from a texel center:
// Catmull-Rom (a = -0.5) for bicubic, Lanczos-3 otherwise.
float filter_weight(uint resample, float x) {
    x = abs(x);
    if (resample == FILTER_BICUBIC) {
        if (x < 1.0) {
            return (1.5 * x - 2.5) * x * x + 1.0;
        } else if (x < 2.0) {
            return ((-0.5 * x + 2.5) * x - 4.0) * x + 2.0;
        }
        return 0.0;
    }
    if (x < 1.0e-5) {
        return 1.0;
    } else if (x >= 3.0) {
        return 0.0;
    }
    float px = PI * x;
    return 3.0 * sin(px) * sin(px / 3.0) / (px * px);
}

// Bicubic (4x4 taps) or Lanczos-3 (6x6) at unflipped canvas position `s`,
// weights normalized to sum to one and the result clamped to a byte.
uvec3 filter_separable(uint resample, vec2 s, ivec2 lo, ivec2 hi) {
    int first = resample == FILTER_BICUBIC ? -1 : -2;
    int taps = resample == FILTER_BICUBIC ? 4 : 6;
    vec2 f = s - vec2(0.5);
    vec2 b = floor(f);
    vec2 t = f - b;
    ivec2 base = ivec2(b);
    float wx[6];
    float wy[6];
    float sx = 0.0;
    float sy = 0.0;
    for (int k = 0; k < taps; ++k) {
        wx[k] = filter_weight(resample, float(first + k) - t.x);
        wy[k] = filter_weight(resample, float(first + k) - t.y);
        sx += wx[k];
        sy += wy[k];
    }
    vec3 acc = vec3(0.0);
    for (int j = 0; j < taps; ++j) {
        vec3 row = vec3(0.0);
        for (int i = 0; i < taps; ++i) {
            row += wx[i] * vec3(filter_texel(base + ivec2(first + i, first + j), lo, hi));
        }
        acc += wy[j] * row;
    }
    return uvec3(floor(clamp(acc / (sx * sy), vec3(0.0), vec3(255.0)) + vec3(0.5)));
}

// The area kernel: the rounded integer mean of an nx-by-ny box of texels
// centered on unflipped canvas position `s`.
uvec3 filter_area(vec2 s, ivec2 lo, ivec2 hi) {
    uint nx = ((pc.flags >> 8u) & 0xfu) + 1u;
    uint ny = ((pc.flags >> 12u) & 0xfu) + 1u;
    ivec2 start = ivec2(floor(s - vec2(float(nx), float(ny)) * 0.5 + vec2(0.5)));
    uvec3 sum = uvec3(0u);
    for (uint j = 0u; j < ny; ++j) {
        for (uint i = 0u; i < nx; ++i) {
            sum += filter_texel(start + ivec2(int(i), int(j)), lo, hi);
        }
    }
    uint n = nx * ny;
    return (sum + uvec3(n / 2u)) / uvec3(n);
}

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
            // Sync shapes stay in output-local source-pattern coordinates:
            // the logical raster, which differs from the target only when
            // the slicer presents a transformed (physical) buffer.
            local = unit * vec2(pc.logical_width, pc.logical_height);
            // A nonzero transfer entry means this destination pixel is
            // covered.  Clamp its source to a texel center, including when a
            // partially covered edge maps just outside the source rectangle.
            // Uncovered pixels took the table==0 return above.
            local = clamp(local, vec2(0.5),
                          vec2(float(pc.logical_width) - 0.5,
                               float(pc.logical_height) - 0.5));
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
            // Sync shapes are on the logical raster, so an exact copy that
            // owns a transform hit-tests the logical index, as the warp
            // branch above does through `local`.
            uvec2 q = exact_logical_index(uvec2(p));
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
        // Every kernel but bilinear works on this unflipped position and
        // inverts each tap instead (`filter_texel`).
        vec2 unflipped = source;
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
            uint resample = (pc.flags >> 4u) & 0xfu;
            // The texels the source rectangle touches, unflipped: the
            // range every tap below is clamped to.
            ivec2 last = ivec2(canvas_size) - ivec2(1);
            ivec2 lo = clamp(ivec2(floor(pc.source_rect.xy)), ivec2(0), last);
            ivec2 hi = clamp(ivec2(ceil(pc.source_rect.xy + pc.source_rect.zw)) - ivec2(1),
                             lo, last);
            if (resample == FILTER_NEAREST) {
                c = filter_texel(ivec2(floor(unflipped)), lo, hi);
            } else if (resample == FILTER_BICUBIC || resample == FILTER_LANCZOS) {
                c = filter_separable(resample, unflipped, lo, hi);
            } else if (resample == FILTER_AREA) {
                c = filter_area(unflipped, lo, hi);
            } else {
                vec4 t = textureLod(sampler2D(canvas, canvasSampler),
                                    source / canvas_size, 0.0);
                c = uvec3(round(t.rgb * 255.0));
            }
        }
    } else {
        // The integer copy: physical pixel `p` shows logical pixel `q` of
        // the slice, so an owned transform is a pure texel permutation and
        // never a resample. The transfer table stays physical (`ab` above).
        uvec2 q = exact_logical_index(uvec2(p));
        // Validate unsigned origins before addition or y inversion. An
        // out-of-canvas pixel is opaque black, including with nonzero lift.
        uvec2 canvas_size = uvec2(textureSize(canvas, 0));
        if (pc.source_x >= canvas_size.x || pc.source_y >= canvas_size.y ||
            q.x >= canvas_size.x - pc.source_x ||
            q.y >= canvas_size.y - pc.source_y) {
            outColor = vec4(0.0, 0.0, 0.0, 1.0);
            return;
        }
        int cy = int(pc.source_y) + int(q.y);
        if (pc.y_invert != 0u) {
            cy = int(pc.canvas_height) - 1 - cy;
        }
        vec4 t = texelFetch(sampler2D(canvas, canvasSampler), ivec2(int(pc.source_x) + int(q.x), cy), 0);
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
