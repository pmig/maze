// mazemail.dev: the dot animation in the hero (progressive enhancement, home page only).
// About a thousand dots form the Maze logo, then morph through icons for what Maze does
// and back. Shapes are drawn once into a small offscreen canvas and sampled on a grid;
// each dot is a damped spring toward its target. The springs run on the CPU (well under
// a tenth of a millisecond a frame); drawing is WebGL2: every dot is an instanced quad,
// so a frame is one buffer upload and two draw calls (halos, then dots) whatever the count.
// That buys exact alpha, a soft glow, dots that stretch along their motion and paths
// that curve on the way to the next shape. Dots near the pointer glow and move aside;
// a click or tap sends a ripple. Respects prefers-reduced-motion (a still logo that
// still glows), pauses when off screen or in a background tab, redraws at 30 fps once
// every dot has settled and stays hidden without JavaScript or WebGL2.
(function () {
  var figure = document.querySelector("[data-dots]");
  if (!figure || !window.Path2D) return;
  var canvas = figure.querySelector("canvas");
  var label = figure.querySelector("[data-dots-label]");
  var reduce = window.matchMedia("(prefers-reduced-motion: reduce)");
  // low-power: a decoration must not wake the discrete GPU of a dual-GPU laptop
  var gl = canvas.getContext("webgl2", {
    alpha: true, premultipliedAlpha: true, antialias: false, depth: false, stencil: false,
    powerPreference: "low-power",
  });
  if (!gl) return;

  // Shapes on a 1024 x 1024 board. Each layer: [kind, path, tone] where kind is
  // "fill" or a stroke width, and tone is the dot strength (1 = full ink, "goal" = amber).
  var LOGO_BACK =
    "M204 480 L232 480 A36 36 0 0 1 268 516 A36 36 0 0 0 304 552 L316 552 A36 36 0 0 1 352 588 A36 36 0 0 0 388 624 L636 624 A36 36 0 0 0 672 588 A36 36 0 0 1 708 552 L720 552 A36 36 0 0 0 756 516 A36 36 0 0 1 792 480 L820 480 L820 376 A72 72 0 0 0 748 304 L276 304 A72 72 0 0 0 204 376 Z";
  var LOGO_FRONT =
    "M204 384 L328 384 A36 36 0 0 1 364 420 A36 36 0 0 0 400 456 L412 456 A36 36 0 0 1 448 492 A36 36 0 0 0 484 528 L540 528 A36 36 0 0 0 576 492 A36 36 0 0 1 612 456 L624 456 A36 36 0 0 0 660 420 A36 36 0 0 1 696 384 L820 384 L820 376 A72 72 0 0 0 748 304 L276 304 A72 72 0 0 0 204 376 Z";
  var circle = function (x, y, r) {
    return "M" + (x - r) + " " + y + " a" + r + " " + r + " 0 1 0 " + 2 * r + " 0 a" + r + " " + r + " 0 1 0 " + -2 * r + " 0 Z";
  };
  var rrect = function (x, y, w, h, r) {
    return "M" + (x + r) + " " + y + " H" + (x + w - r) + " A" + r + " " + r + " 0 0 1 " + (x + w) + " " + (y + r) +
      " V" + (y + h - r) + " A" + r + " " + r + " 0 0 1 " + (x + w - r) + " " + (y + h) + " H" + (x + r) +
      " A" + r + " " + r + " 0 0 1 " + x + " " + (y + h - r) + " V" + (y + r) + " A" + r + " " + r + " 0 0 1 " + (x + r) + " " + y + " Z";
  };

  var SHAPES = [
    { label: "Maze", layers: [
      ["fill", rrect(204, 304, 616, 416, 72), 0.16],
      ["fill", LOGO_BACK, 0.45],
      ["fill", LOGO_FRONT, 1],
      ["fill", circle(512, 576, 44), "goal"],
    ] },
    { label: "Your mail, local-first", layers: [
      [56, rrect(196, 296, 632, 432, 76), 1],
      [56, "M260 360 L512 548 L764 360", 1],
      ["fill", circle(512, 548, 44), "goal"],
    ] },
    { label: "Instant search", layers: [
      [64, circle(446, 446, 196), 1],
      [96, "M598 598 L796 796", 1],
      ["fill", circle(446, 446, 52), "goal"],
    ] },
    { label: "GPU-rendered, fast", layers: [
      ["fill", "M586 150 L268 566 H492 L430 874 L756 438 H530 Z", 1],
      ["fill", circle(800, 220, 40), "goal"],
    ] },
    { label: "Encrypted at rest", layers: [
      [60, "M370 470 V376 A142 142 0 0 1 654 376 V470", 1],
      ["fill", rrect(270, 460, 484, 384, 64), 0.9],
      ["fill", circle(512, 628, 48), "goal"],
    ] },
    { label: "Keys stay in your keychain", layers: [
      [60, circle(342, 512, 132), 1],
      [60, "M474 512 H836 M720 512 V620 M816 512 V600", 1],
      ["fill", circle(342, 512, 50), "goal"],
    ] },
    { label: "Scriptable: the mazemail CLI", layers: [
      [52, rrect(180, 250, 664, 524, 64), 1],
      [60, "M300 410 L410 512 L300 614", 1],
      ["fill", rrect(470, 584, 170, 52, 26), "goal"],
    ] },
    { label: "Agents next to your mail", layers: [
      ["fill", "M480 170 C512 420 568 476 818 508 C568 540 512 596 480 846 C448 596 392 540 142 508 C392 476 448 420 480 170 Z", 1],
      ["fill", "M780 168 C790 244 806 260 882 270 C806 280 790 296 780 372 C770 296 754 280 678 270 C754 260 770 244 780 168 Z", "goal"],
    ] },
  ];

  var HOLD = 2600, MORPH = 1300; // ms a shape stays, ms the hand-over is staggered over
  var SWIRL = 2.4; // sideways kick when a dot leaves for a new target (1/s): paths curve
  var STREAK = 0.0024; // stretch per px/s of speed (capped in the shader)
  var RIPPLE = 1100; // ms a click ripple lives
  var IDLE = 1000 / 30; // ms between redraws once every dot has settled
  var dots = [], targets = [], ripples = [], shape = 0, nextAt = 0, size = 0, step = 0, radius = 0;
  var dpr = 1, offX = 0, offY = 0, cssW = 1, cssH = 1, drawnAt = 0, moved = false, glowing = false;
  var running = false, visible = false, raf = 0, last = 0;
  var pointer = { x: -1e4, y: -1e4 };
  var ink = [0, 0, 0], halo = [0, 0, 0], amber = [1, 0.72, 0.42], tint = 1;

  // Each dot is a quad around its centre. The vertex shader places it (wobble, stretch
  // along the velocity, the dot's radius growing with its glow) and skips dots with
  // nothing to draw; the fragment shader draws an antialiased disc (pass 1) or a
  // gaussian halo (pass 0). Output is premultiplied alpha, blended "over".
  var VERT = [
    "#version 300 es",
    "layout(location = 0) in vec4 a_move;  // x, y on the board (CSS px), velocity (px/s)",
    "layout(location = 1) in vec4 a_look;  // alpha, glow, wobble phase, goal (0 or 1)",
    "uniform vec4 u_view;  // board origin in the canvas (CSS px), 2 / canvas size (CSS px)",
    "uniform vec4 u_dot;   // radius (CSS px), device px per CSS px, wobble (CSS px), streak",
    "uniform float u_time;",
    "uniform float u_pass; // 0 halos, 1 dots",
    "out vec2 v_p;         // position in the quad, in radii, with the stretch taken out",
    "flat out vec4 v_look; // alpha, glow, core radius (radii), goal",
    "flat out vec2 v_px;   // one device pixel (radii), fade for the stretch",
    "void main() {",
    "  float a = a_look.x, g = a_look.y * min(1.0, a * 3.0); // hidden dots don't glow",
    "  float core = 1.0 + 0.45 * g, px = 1.0 / (u_dot.x * u_dot.y);",
    "  float ext = u_pass < 0.5 ? (g > 0.01 ? 4.2 : 0.0) : (max(a, g) > 0.003 ? core + px : 0.0);",
    "  if (ext == 0.0) { gl_Position = vec4(0.0, 0.0, 2.0, 1.0); return; } // clipped",
    "  vec2 q = (vec2(gl_VertexID & 1, gl_VertexID >> 1) * 2.0 - 1.0) * ext;",
    "  float speed = length(a_move.zw), s = min(speed * u_dot.w, 2.2);",
    "  vec2 dir = speed > 1.0 ? a_move.zw / speed : vec2(1.0, 0.0);",
    "  vec2 off = (dir * q.x * (1.0 + s) + vec2(-dir.y, dir.x) * q.y) * u_dot.x;",
    "  vec2 wob = vec2(sin(u_time * 1.3 + a_look.z), cos(u_time * 1.1 + a_look.z)) * u_dot.z;",
    "  vec2 p = u_view.xy + a_move.xy + wob + off;",
    "  gl_Position = vec4(p.x * u_view.z - 1.0, 1.0 - p.y * u_view.w, 0.0, 1.0);",
    "  v_p = q;",
    "  v_look = vec4(a, g, core, a_look.w);",
    "  v_px = vec2(px, 1.0 / (1.0 + 0.6 * s)); // a streak spreads the same ink thinner",
    "}",
  ].join("\n");
  var FRAG = [
    "#version 300 es",
    "precision mediump float;",
    "in vec2 v_p;",
    "flat in vec4 v_look;",
    "flat in vec2 v_px;",
    "uniform vec3 u_ink, u_halo, u_amber;",
    "uniform float u_tint; // 1: glowing dots turn amber (light theme, where dark ink can't glow)",
    "uniform highp float u_pass; // shared with the vertex shader, so the same precision",
    "out vec4 o;",
    "void main() {",
    "  float d = length(v_p), a = v_look.x, g = v_look.y, k;",
    "  bool goal = v_look.w > 0.5;",
    "  vec3 c;",
    "  if (u_pass < 0.5) {",
    "    k = (0.34 + 0.22 * u_tint) * g * exp(-0.2 * d * d); // amber on white needs more",
    "    c = goal ? u_amber : u_halo;",
    "  } else {",
    "    k = clamp((v_look.z - d) / v_px.x + 0.5, 0.0, 1.0) * (a + (1.0 - a) * g);",
    "    c = goal ? u_amber : mix(u_ink, u_amber, u_tint * g);",
    "  }",
    "  k *= v_px.y;",
    "  o = vec4(c * k, k);",
    "}",
  ].join("\n");

  var loc = {}, data = new Float32Array(0), cap = 0, glOk = false;
  function setup() {
    glOk = false;
    var p = gl.createProgram();
    [[gl.VERTEX_SHADER, VERT], [gl.FRAGMENT_SHADER, FRAG]].forEach(function (s) {
      var sh = gl.createShader(s[0]);
      gl.shaderSource(sh, s[1]);
      gl.compileShader(sh);
      gl.attachShader(p, sh);
    });
    gl.linkProgram(p);
    if (!gl.getProgramParameter(p, gl.LINK_STATUS)) return false;
    gl.useProgram(p);
    ["u_view", "u_dot", "u_time", "u_pass", "u_ink", "u_halo", "u_amber", "u_tint"].forEach(function (n) {
      loc[n] = gl.getUniformLocation(p, n);
    });
    // one interleaved buffer, 8 floats per dot, advanced once per quad (instanced)
    gl.bindVertexArray(gl.createVertexArray());
    gl.bindBuffer(gl.ARRAY_BUFFER, gl.createBuffer());
    for (var i = 0; i < 2; i++) {
      gl.enableVertexAttribArray(i);
      gl.vertexAttribPointer(i, 4, gl.FLOAT, false, 32, i * 16);
      gl.vertexAttribDivisor(i, 1);
    }
    gl.enable(gl.BLEND);
    gl.blendFunc(gl.ONE, gl.ONE_MINUS_SRC_ALPHA);
    gl.clearColor(0, 0, 0, 0);
    cap = 0;
    glOk = true;
    return true;
  }

  // Morton (Z-order) code: sorting by it keeps neighbouring dots together, so dot i
  // lands in a similar region of every shape and the morph reads as a flow.
  function morton(x, y) {
    var z = 0;
    for (var b = 0; b < 10; b++) z |= ((x >> b) & 1) << (2 * b) | ((y >> b) & 1) << (2 * b + 1);
    return z;
  }

  function sample(s) {
    var grid = Math.floor(size / step), sup = 3, px = grid * sup;
    var off = document.createElement("canvas");
    off.width = off.height = px;
    var o = off.getContext("2d", { willReadFrequently: true });
    o.scale(px / 1024, px / 1024);
    o.lineCap = o.lineJoin = "round";
    s.layers.forEach(function (l) {
      var p = new Path2D(l[1]);
      var color = l[2] === "goal" ? "rgb(255,0,0)" : "rgba(255,255,255," + l[2] + ")";
      if (l[0] === "fill") { o.fillStyle = color; o.fill(p); }
      else { o.strokeStyle = color; o.lineWidth = l[0]; o.stroke(p); }
    });
    var data = o.getImageData(0, 0, px, px).data, plain = [], goal = [];
    var offset = (size - grid * step) / 2 + step / 2;
    for (var gy = 0; gy < grid; gy++)
      for (var gx = 0; gx < grid; gx++) {
        var i = ((gy * sup + 1) * px + gx * sup + 1) * 4, a = data[i + 3] / 255;
        if (a < 0.12) continue;
        var t = { x: offset + gx * step, y: offset + gy * step, a: a, m: morton(gx, gy) };
        (data[i] > 200 && data[i + 1] < 90 ? goal : plain).push(t);
      }
    var byM = function (p, q) { return p.m - q.m; };
    return { ink: plain.sort(byM), goal: goal.sort(byM) };
  }

  function layout() {
    var rect = canvas.getBoundingClientRect();
    if (!rect.width) return false;
    var old = size;
    dpr = Math.min(window.devicePixelRatio || 1, 2);
    size = Math.min(rect.width, rect.height);
    cssW = rect.width;
    cssH = rect.height;
    offX = (rect.width - size) / 2;
    offY = (rect.height - size) / 2;
    canvas.width = Math.round(rect.width * dpr);
    canvas.height = Math.round(rect.height * dpr);
    step = Math.max(5, size / 64);
    radius = step * 0.34;
    targets = SHAPES.map(sample);
    var nInk = 0, nGoal = 0;
    targets.forEach(function (t) { nInk = Math.max(nInk, t.ink.length); nGoal = Math.max(nGoal, t.goal.length); });
    var scale = old ? size / old : 1;
    var fresh = [];
    for (var i = 0; i < nInk + nGoal; i++) {
      var d = dots[i];
      if (!d) {
        // new dots start scattered and invisible, resting where they are until assigned
        d = { x: Math.random() * size, y: Math.random() * size, vx: 0, vy: 0, a: 0, ta: 0, g: 0,
          k: 0.75 + Math.random() * 0.5, ph: Math.random() * 6.283 };
        d.tx = d.x;
        d.ty = d.y;
      }
      if (dots[i]) { d.x *= scale; d.y *= scale; }
      d.goal = i >= nInk;
      d.rank = d.goal ? (i - nInk) / Math.max(1, nGoal) : i / Math.max(1, nInk);
      fresh.push(d);
    }
    dots = fresh;
    assign(shape, true);
    return true;
  }

  // Point every dot at the shape's targets. Dot i takes target floor(i * n / pool), so a
  // shape with fewer targets than dots doubles some up; the duplicates fade out.
  function assign(index, now) {
    var t = targets[index], nInk = 0;
    dots.forEach(function (d) { if (!d.goal) nInk++; });
    var t0 = performance.now();
    dots.forEach(function (d, i) {
      var list = d.goal ? t.goal : t.ink, pool = d.goal ? dots.length - nInk : nInk, j = d.goal ? i - nInk : i;
      if (!list.length) { d.next = { x: size / 2, y: size / 2, a: 0 }; }
      else {
        var k = Math.floor((j * list.length) / pool), first = Math.floor(((j - 1) * list.length) / pool) !== k || j === 0;
        var p = list[k];
        d.next = { x: p.x, y: p.y, a: first ? p.a : 0 };
      }
      d.at = now ? t0 : t0 + d.rank * MORPH * 0.7 + Math.random() * MORPH * 0.3;
    });
  }

  // CSS colours in any syntax (hex, rgb(), oklch(), …) to 0..1 sRGB, through a
  // one-pixel 2D canvas so the browser does the parsing.
  var probe = document.createElement("canvas");
  probe.width = probe.height = 1;
  var pctx = probe.getContext("2d", { willReadFrequently: true });
  function rgb(css, fallback) {
    if (!css) return fallback;
    pctx.clearRect(0, 0, 1, 1);
    pctx.fillStyle = "#000";
    pctx.fillStyle = css;
    pctx.fillRect(0, 0, 1, 1);
    var p = pctx.getImageData(0, 0, 1, 1).data;
    return [p[0] / 255, p[1] / 255, p[2] / 255];
  }

  function colors() {
    var cs = getComputedStyle(figure);
    ink = rgb(cs.color, ink);
    amber = rgb(cs.getPropertyValue("--amber").trim(), amber);
    // light dots (dark theme) glow in their own colour; dark dots would only smudge, so
    // in the light theme the glow is amber and glowing dots turn amber
    var light = 0.2126 * ink[0] + 0.7152 * ink[1] + 0.0722 * ink[2] > 0.5;
    halo = light ? ink : amber;
    tint = light ? 0 : 1;
    if (ready && !running) frame(performance.now());
  }

  function frame(now) {
    raf = 0;
    var dt = Math.min(0.05, (now - (last || now)) / 1000);
    last = now;
    var still = reduce.matches;
    if (!still && now >= nextAt) {
      shape = (shape + 1) % SHAPES.length;
      assign(shape, false);
      nextAt = now + HOLD + MORPH;
      if (label) {
        label.classList.add("is-out");
        setTimeout(function () { label.textContent = SHAPES[shape].label; label.classList.remove("is-out"); }, 220);
      }
    }
    // click ripples: a ring that grows from the click, pushing dots outward and lighting
    // them as it passes
    var rings = [];
    for (var r = ripples.length - 1; r >= 0; r--) {
      var age = (now - ripples[r].t) / RIPPLE;
      if (age >= 1 || still) { ripples.splice(r, 1); continue; }
      rings.push({ x: ripples[r].x, y: ripples[r].y, at: age * size * 0.9, w: size * 0.07, f: (1 - age) * (1 - age) });
    }
    var busy = rings.length > 0 || moved;
    moved = false;
    glowing = false;
    for (var i = 0; i < dots.length; i++) {
      var d = dots[i];
      if (d.next && (still || now >= d.at)) {
        d.tx = d.next.x; d.ty = d.next.y; d.ta = d.next.a; d.next = null;
        // a sideways kick, always the same way round: each dot curves to its new place
        // and the whole morph turns like a slow vortex
        if (!still) { d.vx -= (d.ty - d.y) * SWIRL; d.vy += (d.tx - d.x) * SWIRL; }
      }
      // glow: visible dots near the pointer light up (eased in and out)
      var gx = d.x - pointer.x, gy = d.y - pointer.y, gd = Math.sqrt(gx * gx + gy * gy) / (size * 0.2), gt = 0;
      if (gd < 1) gt = (1 - gd) * (1 - gd) * (1 + 2 * gd);
      var ax = 0, ay = 0, rg = 0;
      for (var q = 0; q < rings.length; q++) {
        var R = rings[q], rx = d.x - R.x, ry = d.y - R.y, rd = Math.sqrt(rx * rx + ry * ry) || 1;
        var band = 1 - Math.abs(rd - R.at) / R.w;
        if (band <= 0) continue;
        band *= band * R.f;
        rg = Math.max(rg, band);
        ax += (rx / rd) * band * 2400;
        ay += (ry / rd) * band * 2400;
      }
      // the pointer's glow eases in and out; a ripple lights dots at once, then fades
      var g0 = d.g;
      d.g = still ? gt : Math.max(rg, g0 + (gt - g0) * Math.min(1, dt * 10));
      if (d.g > 0.01) glowing = true;
      if (still) { d.x = d.tx; d.y = d.ty; d.a = d.ta; d.vx = d.vy = 0; continue; }
      var K = 70 * d.k, D = 13;
      ax += (d.tx - d.x) * K - d.vx * D;
      ay += (d.ty - d.y) * K - d.vy * D;
      var px = d.x - pointer.x, py = d.y - pointer.y, r2 = px * px + py * py, RP = size * 0.12;
      if (r2 < RP * RP && r2 > 0.01) {
        var f = (1 - Math.sqrt(r2) / RP) * 9000 / Math.sqrt(r2);
        ax += px * f * 0.02; ay += py * f * 0.02;
      }
      d.vx += ax * dt; d.vy += ay * dt;
      d.x += d.vx * dt; d.y += d.vy * dt;
      d.a += (d.ta - d.a) * Math.min(1, dt * 7);
      if (!busy && (d.next || Math.abs(d.g - g0) > 0.001 || Math.abs(d.vx) + Math.abs(d.vy) > 2 || Math.abs(d.ta - d.a) > 0.004)) busy = true;
    }
    // once everything has settled (a resting pointer included) only the sub-pixel wobble
    // moves: 30 fps is plenty
    if (busy || still || !running || now - drawnAt >= IDLE - 2) {
      draw(now);
      drawnAt = now;
    }
    if (running && !still) raf = requestAnimationFrame(frame);
    else running = false; // reduced motion: one still frame, the next start() redraws
  }

  function draw(now) {
    if (!glOk) return;
    var n = dots.length;
    if (cap < n) {
      cap = n;
      data = new Float32Array(n * 8);
      gl.bufferData(gl.ARRAY_BUFFER, data.byteLength, gl.DYNAMIC_DRAW);
    }
    for (var i = 0, o = 0; i < n; i++, o += 8) {
      var d = dots[i];
      data[o] = d.x; data[o + 1] = d.y; data[o + 2] = d.vx; data[o + 3] = d.vy;
      data[o + 4] = d.a; data[o + 5] = d.g; data[o + 6] = d.ph; data[o + 7] = d.goal ? 1 : 0;
    }
    gl.bufferSubData(gl.ARRAY_BUFFER, 0, data, 0, n * 8);
    var still = reduce.matches;
    gl.viewport(0, 0, canvas.width, canvas.height);
    gl.clear(gl.COLOR_BUFFER_BIT);
    gl.uniform4f(loc.u_view, offX, offY, 2 / cssW, 2 / cssH);
    gl.uniform4f(loc.u_dot, radius, dpr, still ? 0 : step * 0.08, still ? 0 : STREAK);
    gl.uniform1f(loc.u_time, (now / 1000) % 3600);
    gl.uniform3fv(loc.u_ink, ink);
    gl.uniform3fv(loc.u_halo, halo);
    gl.uniform3fv(loc.u_amber, amber);
    gl.uniform1f(loc.u_tint, tint);
    // halos first, so no halo veils a neighbouring dot; goal dots come last in the buffer
    // and so sit on top
    if (glowing) {
      gl.uniform1f(loc.u_pass, 0);
      gl.drawArraysInstanced(gl.TRIANGLE_STRIP, 0, 4, n);
    }
    gl.uniform1f(loc.u_pass, 1);
    gl.drawArraysInstanced(gl.TRIANGLE_STRIP, 0, 4, n);
  }

  function start() {
    if (running || !visible || document.hidden || !glOk) return;
    running = true;
    last = 0;
    raf = requestAnimationFrame(frame);
  }
  function stop() {
    running = false;
    if (raf) cancelAnimationFrame(raf);
    raf = 0;
  }

  // Set up on the first time the canvas has a size: below 1000 px it is hidden, so a page
  // opened on a narrow window starts the animation only once it is widened.
  var ready = false;
  function ensure() {
    if (ready) return true;
    if (!layout()) return false;
    ready = true;
    nextAt = performance.now() + HOLD;
    if (label) label.textContent = SHAPES[0].label;
    // first appearance: dots drift in from scattered positions into the logo
    dots.forEach(function (d) { d.at = performance.now() + Math.random() * 600; });
    return true;
  }

  if (!setup()) return; // shaders didn't build: keep the figure hidden
  figure.hidden = false;
  colors();
  ensure();

  // The GPU can drop the context (driver reset, too many contexts in other tabs): stop,
  // and rebuild the program and buffer when the browser hands it back.
  canvas.addEventListener("webglcontextlost", function (e) { e.preventDefault(); glOk = false; stop(); });
  canvas.addEventListener("webglcontextrestored", function () {
    if (!setup()) return;
    start();
    if (!running && ready) frame(performance.now());
  });

  new IntersectionObserver(function (entries) {
    visible = entries[0].isIntersecting;
    if (!visible || !ensure()) return stop();
    start();
    if (reduce.matches) frame(performance.now());
  }).observe(canvas);
  document.addEventListener("visibilitychange", function () { document.hidden ? stop() : start(); });
  // After a resize: re-sample the shapes at the new size and make sure the animation runs
  // (start() still waits for the canvas to be on screen and the tab to be visible).
  new ResizeObserver(function () {
    var ok = ready ? layout() : ensure();
    if (!ok) return stop();
    start();
    if (!running) frame(performance.now());
  }).observe(canvas);
  new MutationObserver(colors).observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme"] });
  window.matchMedia("(prefers-color-scheme: dark)").addEventListener("change", colors);
  reduce.addEventListener("change", function () { stop(); shape = 0; assign(0, true); start(); frame(performance.now()); });
  // Under reduced motion there is no loop: redraw the still logo so the glow follows.
  function still() {
    if (reduce.matches && ready && !raf) raf = requestAnimationFrame(frame);
  }
  function local(e) {
    var r = canvas.getBoundingClientRect();
    return { x: e.clientX - r.left - (r.width - size) / 2, y: e.clientY - r.top - (r.height - size) / 2 };
  }
  canvas.addEventListener("pointermove", function (e) {
    var p = local(e);
    pointer.x = p.x;
    pointer.y = p.y;
    moved = true;
    still();
  });
  canvas.addEventListener("pointerleave", function () { pointer.x = pointer.y = -1e4; moved = true; still(); });
  canvas.addEventListener("pointerdown", function (e) {
    if (reduce.matches || !ready) return;
    var p = local(e);
    ripples.push({ x: p.x, y: p.y, t: last || performance.now() }); // on the frames' clock
    if (ripples.length > 3) ripples.shift();
  });
})();
