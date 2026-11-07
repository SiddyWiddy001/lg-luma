uniform sampler2D u_texture;
uniform float u_aa;
uniform vec4 u_edge_fade;
uniform vec4 u_edge_fade_x;
uniform vec3 u_app_top;
uniform vec3 u_app_bottom;
uniform bool u_app;
uniform float u_icon_tint;
uniform vec4 u_glass;
varying PAINT_PRECISION vec2 v_uv;
varying PAINT_PRECISION vec4 v_color;
varying PAINT_PRECISION vec4 v_shape;
varying PAINT_PRECISION vec2 v_params;
varying PAINT_PRECISION float v_logical_y;
varying PAINT_PRECISION float v_logical_x;
float edge_coverage() {
    float edge = 1.0;
    if (u_edge_fade.y > u_edge_fade.x)
        edge = smoothstep(u_edge_fade.x, u_edge_fade.y, v_logical_y) *
            (1.0 - smoothstep(u_edge_fade.z, u_edge_fade.w, v_logical_y));
    if (u_edge_fade_x.y > u_edge_fade_x.x)
        edge *= smoothstep(u_edge_fade_x.x, u_edge_fade_x.y, v_logical_x);
    if (u_edge_fade_x.w > u_edge_fade_x.z)
        edge *= 1.0 - smoothstep(u_edge_fade_x.z, u_edge_fade_x.w, v_logical_x);
    return edge;
}
// Straight-alpha "over": the paint pass blends with SRC_ALPHA, so colours stay unpremultiplied.
vec4 over(vec4 fg, vec4 bg) {
    float a = fg.a + bg.a * (1.0 - fg.a);
    return vec4((fg.rgb * fg.a + bg.rgb * bg.a * (1.0 - fg.a)) / max(a, 0.0001), a);
}
void main() {
    if (v_params.x < 0.0) {
        vec4 color = texture2D(u_texture, v_uv) * v_color;
        // -2: wallpaper with folded full-screen fills, p * m + k (k in v_shape).
        if (v_params.x < -1.5)
            color.rgb += v_shape.rgb;
        gl_FragColor = color * vec4(1.0, 1.0, 1.0, edge_coverage());
        return;
    }
    float blur = max(-v_params.y, 0.0);
    // A glow (Draw::Glow) marks its half sizes negative: it uses a continuous exponential
    // falloff instead of the shadow's symmetric smoothstep.
    bool glow = v_shape.z < 0.0;
    vec2 half_size = abs(v_shape.zw) - vec2(blur);
    float radius = min(v_params.x, min(half_size.x, half_size.y));
    vec2 q = abs(v_shape.xy) - half_size + vec2(radius);
    float d = length(max(q, vec2(0.0))) + min(max(q.x, q.y), 0.0) - radius;
    // Box-filtered coverage of one drawable pixel (u_aa) centred on the edge: an edge on a
    // pixel boundary stays one crisp step, and a one-pixel border is fully opaque.
    float alpha = clamp(0.5 - d / u_aa, 0.0, 1.0);
    if (blur > 0.0) {
        if (glow) {
            // Full strength inside the card and out to `hold`, then a smooth exponential
            // decay. The stacked flat-alpha rings this replaced stepped hard every few pixels
            // and banded; this curve is deliberately weaker than they were, both at the card
            // edge and as it falls off, so the halo stays close to the tile instead of
            // brightening the wallpaper around it. Subtracting the residual at the quad's edge
            // keeps the halo at exactly zero where the geometry ends, so the cutoff cannot show
            // as a faint ring.
            float hold = blur * 0.1;
            float decay = max(blur * 0.2, 0.001);
            float edge = exp(-max(blur - hold, 0.0) / decay);
            alpha = (exp(-max(d - hold, 0.0) / decay) - edge) / (1.0 - edge);
        } else {
            alpha = 1.0 - smoothstep(-blur, blur, d);
        }
    }
    if (v_params.y > 0.0) {
        alpha *= clamp(0.5 + (d + v_params.y) / u_aa, 0.0, 1.0);
    }
    if (u_glass.w > 0.5) {
        // One analytic pass for the smoked-glass plate: vertical tint (v_color), a soft 3px
        // inner rim, a crisp 1px rim and a top reflection that fades to the bottom edge.
        float soft = clamp(0.5 + (d + 3.0) / u_aa, 0.0, 1.0) * u_glass.y;
        float rim = clamp(0.5 + (d + 1.0) / u_aa, 0.0, 1.0) * u_glass.x;
        float y = v_shape.y / (2.0 * half_size.y) + 0.5;
        float reflection = u_glass.z * (1.0 - y);
        // Scale every layer by the rounded-rect coverage, exactly as the four separate draws did.
        float cov = alpha;
        vec4 plate = vec4(v_color.rgb, v_color.a * cov);
        plate = over(vec4(1.0, 1.0, 1.0, soft * cov), plate);
        plate = over(vec4(1.0, 1.0, 1.0, rim * cov), plate);
        plate = over(vec4(1.0, 1.0, 1.0, reflection * cov), plate);
        gl_FragColor = plate * vec4(1.0, 1.0, 1.0, edge_coverage());
        return;
    }
    vec4 color = texture2D(u_texture, v_uv);
    if (u_app) {
        float y = v_shape.y / (2.0 * half_size.y) + 0.5;
        vec3 background = mix(u_app_top, u_app_bottom, y);
        float inside = step(0.0, v_uv.x) * step(v_uv.x, 1.0) *
            step(0.0, v_uv.y) * step(v_uv.y, 1.0);
        color = vec4(mix(background, color.rgb, color.a * inside), 1.0);
    }
    if (u_icon_tint > 0.0) {
        // Cool monochrome for installed icons: luma drives a dark-mode ramp from near
        // black to a soft highlight, so every tile reads as part of the same glass set.
        float luma = dot(color.rgb, vec3(0.2126, 0.7152, 0.0722));
        vec3 mono = vec3(luma) * vec3(0.82, 0.86, 0.93);
        color.rgb = mix(color.rgb, mono, u_icon_tint);
    }
    gl_FragColor = color * v_color * vec4(1.0, 1.0, 1.0, alpha * edge_coverage());
}
