precision mediump float;

varying vec2 uv;

uniform float uTime;
uniform float uAudioLevel;
uniform float uRadius;
uniform float uAspect;
uniform float uAlpha;

void main() {
    // Centered coordinates; x is scaled so the orb stays round on a 16:9 screen.
    vec2 p = vec2((uv.x - 0.5) * uAspect, uv.y - 0.5);
    float r = length(p);

    float level = clamp(uAudioLevel, 0.0, 1.0);
    float pulse = 0.5 + 0.5 * sin(uTime * 4.0);
    float radius = uRadius * (1.0 + 0.18 * pulse + 0.35 * level);

    // Solid emissive core with a soft edge, plus an exponential halo.
    float core = 1.0 - smoothstep(radius - 0.012, radius, r);
    float glow = exp(-abs(r - radius) * 18.0);

    vec3 color = vec3(0.22, 0.85, 1.0);
    float alpha = (core * 0.85 + glow * (0.45 + 0.4 * level)) * uAlpha;
    // Premultiplied, so additive blending never tints the scene.
    gl_FragColor = vec4(color * alpha, alpha);
}
