// Fullscreen triangle, no vertex buffers: three synthesized clip-space
// positions covering the whole viewport, so a single draw call blends
// whatever rectangle `vkCmdSetViewport`/`vkCmdSetScissor` set for this job.
// Rebuild with `naga blend.vert blend.vert.spv` (naga is at ~/.cargo/bin/naga
// in WSL) whenever this file changes, and check the .spv in alongside it —
// the target machines have no shader compiler.
#version 450

void main() {
    vec2 pos = vec2((gl_VertexIndex << 1) & 2, gl_VertexIndex & 2);
    gl_Position = vec4(pos * 2.0 - 1.0, 0.0, 1.0);
}
