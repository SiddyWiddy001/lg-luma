varying PAINT_PRECISION vec4 v_color;
varying PAINT_PRECISION float v_coverage;
varying PAINT_PRECISION float v_logical_y;
varying PAINT_PRECISION float v_logical_x;
uniform vec4 u_edge_fade;
uniform vec4 u_edge_fade_x;
uniform float u_mono;
void main() {
    float edge = 1.0;
    if (u_edge_fade.y > u_edge_fade.x) {
        edge = smoothstep(u_edge_fade.x, u_edge_fade.y, v_logical_y) *
            (1.0 - smoothstep(u_edge_fade.z, u_edge_fade.w, v_logical_y));
    }
    if (u_edge_fade_x.y > u_edge_fade_x.x)
        edge *= smoothstep(u_edge_fade_x.x, u_edge_fade_x.y, v_logical_x);
    if (u_edge_fade_x.w > u_edge_fade_x.z)
        edge *= 1.0 - smoothstep(u_edge_fade_x.z, u_edge_fade_x.w, v_logical_x);
    vec3 rgb = v_color.rgb;
    if (u_mono > 0.0) {
        // Brand artwork becomes a cool monochrome mark. The ramp lifts dark brand colours
        // (Netflix red, navy, black) so every logo stays visible on the smoked plate,
        // while white marks keep their highlights.
        float luma = dot(rgb, vec3(0.2126, 0.7152, 0.0722));
        vec3 mono = vec3(mix(0.50, 0.96, smoothstep(0.02, 0.72, luma)));
        rgb = mix(rgb, mono, u_mono);
    }
    gl_FragColor = vec4(rgb, v_color.a * clamp(v_coverage, 0.0, 1.0) * edge);
}
