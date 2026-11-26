import Box from '@mui/material/Box';
import { useEffect, useRef } from 'react';

/** Blocks per screen height, and seconds a touched block stays lit: as the shader has them. */
const CELLS = 12;
const LIT = 2.0;

const VERTEX = `
attribute vec2 a;
void main() { gl_Position = vec4(a, 0.0, 1.0); }
`;

/**
 * Soft monochrome fog flowing through a domain-warped noise field, with a
 * faint grid of rounded blocks that slowly emerge and dissolve, a vignette
 * and a little film grain. A block the pointer touches lights up and fades.
 */
const FRAGMENT = `
#ifdef GL_FRAGMENT_PRECISION_HIGH
precision highp float;
#else
precision mediump float;
#endif
uniform vec2 r;
uniform float t;
/** The last blocks touched: their cell (x, y) and when the highlight began (z). */
uniform vec3 lit[4];

float hash(vec2 p) {
  p = fract(p * vec2(123.34, 456.21));
  p += dot(p, p + 45.32);
  return fract(p.x * p.y);
}

float noise(vec2 p) {
  vec2 i = floor(p);
  vec2 f = fract(p);
  vec2 u = f * f * (3.0 - 2.0 * f);
  return mix(mix(hash(i), hash(i + vec2(1.0, 0.0)), u.x),
             mix(hash(i + vec2(0.0, 1.0)), hash(i + 1.0), u.x), u.y);
}

/** A rounded box's signed distance: centre-relative point, half size, corner radius. */
float rbox(vec2 q, vec2 size, float radius) {
  vec2 d = abs(q) - size + radius;
  return length(max(d, 0.0)) + min(max(d.x, d.y), 0.0) - radius;
}

/** Rotations about x and y by an angle. */
float fbm(vec2 p) {
  float v = 0.0;
  float a = 0.5;
  for (int k = 0; k < 4; k++) {
    v += a * noise(p);
    p = p * 2.03 + vec2(1.7, 9.2);
    a *= 0.5;
  }
  return v;
}

void main() {
  vec2 uv = gl_FragCoord.xy / r.y;
  float s = t * 0.02;

  vec2 q = vec2(fbm(uv * 1.6 + s), fbm(uv * 1.6 - s + 4.3));
  float fog = fbm(uv * 1.8 + 1.6 * q + vec2(s * 1.5, -s));

  float cells = ${CELLS.toFixed(1)};
  vec2 g = uv * cells + vec2(s * 4.0, 0.0);
  vec2 id = floor(g);
  vec2 lp = fract(g) - 0.5;
  float h = 0.4;

  // How far this block is through its highlight, 0..1; 0 when untouched.
  float p = 0.0;
  for (int k = 0; k < 4; k++)
    if (all(equal(lit[k].xy, id))) p = clamp((t - lit[k].z) / ${LIT.toFixed(1)}, 0.0, 1.0);
  float glow = sin(3.1416 * p);

  // A rounded square; one the pointer touched lights up and fades again.
  float aa = 1.5 * cells / r.y;
  float box = rbox(lp, vec2(h), 0.08);
  float fill = 1.0 - smoothstep(0.0, aa, box);
  float edge = 1.0 - smoothstep(0.0, aa, abs(box));

  float life = smoothstep(0.5, 0.72, fbm(id * 0.15 + vec2(s * 3.0, -s * 2.0)));
  float pulse = 0.5 + 0.5 * sin(t * 0.22 + hash(id) * 6.2832);
  float on = max(life * smoothstep(0.35, 0.95, pulse), glow);

  float c = fog * fog * 0.5;
  c += edge * (0.025 + on * 0.3) + fill * on * (0.1 + 0.12 * glow);

  vec2 v = gl_FragCoord.xy / r - 0.5;
  c *= 1.0 - dot(v, v) * 1.1;
  c += (hash(gl_FragCoord.xy + fract(t * 0.37) * 97.0) - 0.5) * 0.025;
  gl_FragColor = vec4(vec3(c), 1.0);
}
`;

/** A compiled and linked program, or null when the GPU refuses it. */
function program(gl: WebGLRenderingContext): WebGLProgram | null {
  const linked = gl.createProgram();
  for (const [kind, source] of [
    [gl.VERTEX_SHADER, VERTEX],
    [gl.FRAGMENT_SHADER, FRAGMENT],
  ] as const) {
    const shader = gl.createShader(kind);
    if (!shader) return null;
    gl.shaderSource(shader, source);
    gl.compileShader(shader);
    if (!gl.getShaderParameter(shader, gl.COMPILE_STATUS)) return null;
    gl.attachShader(linked, shader);
  }
  gl.linkProgram(linked);
  return gl.getProgramParameter(linked, gl.LINK_STATUS) ? linked : null;
}

/**
 * The login scene's backdrop: a full-size WebGL canvas, black and white.
 * One still frame under `prefers-reduced-motion`, paused while the tab is
 * hidden; without WebGL only the static gradient behind it shows. The block
 * under the pointer lights up (not under reduced motion).
 */
export function Shader() {
  const host = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const box = host.current;
    if (!box) return;
    // A fresh canvas per mount: a context lost on unmount never comes back.
    const canvas = document.createElement('canvas');
    canvas.style.cssText = 'position:absolute;inset:0;width:100%;height:100%;display:block';
    const gl = canvas.getContext('webgl', { alpha: false, antialias: false, depth: false });
    if (!gl) return;
    const linked = program(gl);
    if (!linked) return;
    gl.useProgram(linked);
    gl.bindBuffer(gl.ARRAY_BUFFER, gl.createBuffer());
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 3, -1, -1, 3]), gl.STATIC_DRAW);
    const position = gl.getAttribLocation(linked, 'a');
    gl.enableVertexAttribArray(position);
    gl.vertexAttribPointer(position, 2, gl.FLOAT, false, 0, 0);
    const size = gl.getUniformLocation(linked, 'r');
    const time = gl.getUniformLocation(linked, 't');
    const litAt = gl.getUniformLocation(linked, 'lit');
    // Four slots of (cell x, cell y, start); far-off cells match nothing.
    const touches = new Float32Array(12).fill(-1e5);
    let slot = 0;
    gl.uniform3fv(litAt, touches);
    box.appendChild(canvas);

    const still = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
    const start = performance.now();
    let frame = 0;
    const clock = (now: number) => 40 + (now - start) / 1000;
    const draw = (now: number) => {
      gl.uniform1f(time, clock(now));
      gl.drawArrays(gl.TRIANGLES, 0, 3);
    };
    const loop = (now: number) => {
      draw(now);
      frame = requestAnimationFrame(loop);
    };
    const play = () => {
      cancelAnimationFrame(frame);
      if (!still && document.visibilityState === 'visible') frame = requestAnimationFrame(loop);
    };
    const observer = new ResizeObserver(() => {
      const ratio = Math.min(window.devicePixelRatio || 1, 2);
      canvas.width = Math.max(1, Math.round(canvas.clientWidth * ratio));
      canvas.height = Math.max(1, Math.round(canvas.clientHeight * ratio));
      gl.viewport(0, 0, canvas.width, canvas.height);
      gl.uniform2f(size, canvas.width, canvas.height);
      draw(still ? start : performance.now());
    });
    observer.observe(canvas);
    document.addEventListener('visibilitychange', play);

    // The block under the pointer, found as the shader finds it; a new one lights.
    let touched = '';
    const point = (event: PointerEvent) => {
      const rect = canvas.getBoundingClientRect();
      const x = event.clientX - rect.left;
      const y = rect.bottom - event.clientY;
      if (x < 0 || y < 0 || x > rect.width || y > rect.height) {
        touched = '';
        return;
      }
      const t = clock(performance.now());
      const cx = Math.floor((x / rect.height) * CELLS + t * 0.08);
      const cy = Math.floor((y / rect.height) * CELLS);
      if (`${cx},${cy}` === touched) return;
      touched = `${cx},${cy}`;
      let at = [0, 1, 2, 3].find((k) => touches[k * 3] === cx && touches[k * 3 + 1] === cy);
      if (at !== undefined && t - (touches[at * 3 + 2] ?? 0) < LIT) return;
      if (at === undefined) {
        at = slot;
        slot = (slot + 1) % 4;
      }
      touches.set([cx, cy, t], at * 3);
      gl.uniform3fv(litAt, touches);
    };
    if (!still) window.addEventListener('pointermove', point);
    play();

    return () => {
      cancelAnimationFrame(frame);
      observer.disconnect();
      document.removeEventListener('visibilitychange', play);
      window.removeEventListener('pointermove', point);
      gl.getExtension('WEBGL_lose_context')?.loseContext();
      canvas.remove();
    };
  }, []);

  return (
    <Box
      ref={host}
      aria-hidden
      sx={{
        position: 'absolute',
        inset: 0,
        background: 'radial-gradient(ellipse at 70% 40%, #1c1c1c 0%, #080808 55%, #000 100%)',
      }}
    />
  );
}
