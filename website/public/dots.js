// mazemail.dev: the dot animation in the hero (progressive enhancement, home page only).
// A few thousand dots form the Maze logo, then morph through icons for what Maze does
// and back. One canvas, no dependencies. Shapes are drawn once into a small offscreen
// canvas and sampled on a grid; each dot is a damped spring toward its target.
// Dots near the pointer glow and move aside. Respects prefers-reduced-motion (a still
// logo that still glows), pauses when off screen or in a background tab, and stays
// hidden without JavaScript.
(function () {
  var figure = document.querySelector("[data-dots]");
  if (!figure || !window.Path2D) return;
  var canvas = figure.querySelector("canvas");
  var label = figure.querySelector("[data-dots-label]");
  var ctx = canvas.getContext("2d");
  var reduce = window.matchMedia("(prefers-reduced-motion: reduce)");

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
  var dots = [], targets = [], shape = 0, nextAt = 0, size = 0, step = 0, radius = 0;
  var ink = "#000", halo = "#000", amber = "#FFB86B", running = false, visible = false, raf = 0, last = 0;
  var pointer = { x: -1e4, y: -1e4 };

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
    var o = off.getContext("2d");
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
    var rect = canvas.getBoundingClientRect(), dpr = Math.min(window.devicePixelRatio || 1, 2);
    if (!rect.width) return false;
    var old = size;
    size = Math.min(rect.width, rect.height);
    canvas.width = Math.round(rect.width * dpr);
    canvas.height = Math.round(rect.height * dpr);
    ctx.setTransform(dpr, 0, 0, dpr, (rect.width - size) / 2 * dpr, (rect.height - size) / 2 * dpr);
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
        d = { x: Math.random() * size, y: Math.random() * size, vx: 0, vy: 0, a: 0, ta: 0,
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

  function colors() {
    var cs = getComputedStyle(figure);
    ink = cs.color;
    amber = cs.getPropertyValue("--amber").trim() || amber;
    // light dots (dark theme) glow in their own colour; dark dots would only smudge, so
    // in the light theme the glow is amber
    var rgb = (ink.match(/\d+(\.\d+)?/g) || [0, 0, 0]).map(Number);
    halo = 0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2] > 128 ? ink : amber;
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
    for (var i = 0; i < dots.length; i++) {
      var d = dots[i];
      if (d.next && (still || now >= d.at)) { d.tx = d.next.x; d.ty = d.next.y; d.ta = d.next.a; d.next = null; }
      // glow: visible dots near the pointer light up (eased in and out)
      var gx = d.x - pointer.x, gy = d.y - pointer.y, gd = Math.sqrt(gx * gx + gy * gy) / (size * 0.2), gt = 0;
      if (gd < 1) gt = (1 - gd) * (1 - gd) * (1 + 2 * gd);
      d.g = still ? gt : (d.g || 0) + (gt - (d.g || 0)) * Math.min(1, dt * 10);
      if (still) { d.x = d.tx; d.y = d.ty; d.a = d.ta; continue; }
      var K = 70 * d.k, D = 13;
      var ax = (d.tx - d.x) * K - d.vx * D, ay = (d.ty - d.y) * K - d.vy * D;
      var px = d.x - pointer.x, py = d.y - pointer.y, r2 = px * px + py * py, R = size * 0.12;
      if (r2 < R * R && r2 > 0.01) {
        var f = (1 - Math.sqrt(r2) / R) * 9000 / Math.sqrt(r2);
        ax += px * f * 0.02; ay += py * f * 0.02;
      }
      d.vx += ax * dt; d.vy += ay * dt;
      d.x += d.vx * dt; d.y += d.vy * dt;
      d.a += (d.ta - d.a) * Math.min(1, dt * 7);
    }
    draw(now);
    if (running && !still) raf = requestAnimationFrame(frame);
    else running = false; // reduced motion: one still frame, the next start() redraws
  }

  // Dots are batched by colour and by alpha (8 levels): one path and one fill per bucket.
  // Glowing dots first get a soft halo (4 levels), then a brighter, slightly larger core;
  // in the light theme they also turn amber.
  function draw(now) {
    ctx.save();
    ctx.setTransform(1, 0, 0, 1, 0, 0);
    ctx.clearRect(0, 0, canvas.width, canvas.height);
    ctx.restore();
    var t = now / 1000, wob = reduce.matches ? 0 : step * 0.08;
    for (var i = 0; i < dots.length; i++) {
      var d = dots[i], g = (d.g || 0) * Math.min(1, d.a * 3); // hidden dots don't glow
      d.px = d.x + Math.sin(t * 1.3 + d.ph) * wob;
      d.py = d.y + Math.cos(t * 1.1 + d.ph) * wob;
      d.ga = g;
      d.ea = d.a + (1 - d.a) * g;
      d.r = radius * (1 + 0.45 * g);
    }
    [false, true].forEach(function (goal) {
      ctx.fillStyle = goal ? amber : halo;
      for (var level = 1; level <= 4; level++) {
        var lo = level === 1 ? 0.02 : (level - 1) / 4, hi = level / 4;
        ctx.globalAlpha = 0.22 * hi;
        ctx.beginPath();
        var any = false;
        for (var i = 0; i < dots.length; i++) {
          var d = dots[i];
          if (d.goal !== goal || d.ga <= lo || d.ga > hi) continue;
          var hr = radius * (1.9 + 1.5 * d.ga);
          ctx.moveTo(d.px + hr, d.py);
          ctx.arc(d.px, d.py, hr, 0, 6.2832);
          any = true;
        }
        if (any) ctx.fill();
      }
      ctx.fillStyle = goal ? amber : ink;
      for (var level = 1; level <= 8; level++) {
        var lo = (level - 1) / 8, hi = level / 8;
        ctx.globalAlpha = hi;
        ctx.beginPath();
        var any = false;
        for (var i = 0; i < dots.length; i++) {
          var d = dots[i];
          if (d.goal !== goal || d.ea <= lo || d.ea > hi) continue;
          ctx.moveTo(d.px + d.r, d.py);
          ctx.arc(d.px, d.py, d.r, 0, 6.2832);
          any = true;
        }
        if (any) ctx.fill();
      }
      // light theme: glowing dots turn amber instead of only getting a halo
      if (goal || halo !== amber) return;
      ctx.fillStyle = amber;
      for (var level = 1; level <= 4; level++) {
        var lo = level === 1 ? 0.02 : (level - 1) / 4, hi = level / 4;
        ctx.globalAlpha = hi;
        ctx.beginPath();
        var any = false;
        for (var i = 0; i < dots.length; i++) {
          var d = dots[i];
          if (d.goal || d.ga <= lo || d.ga > hi) continue;
          ctx.moveTo(d.px + d.r, d.py);
          ctx.arc(d.px, d.py, d.r, 0, 6.2832);
          any = true;
        }
        if (any) ctx.fill();
      }
    });
    ctx.globalAlpha = 1;
  }

  function start() {
    if (running || !visible || document.hidden) return;
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

  figure.hidden = false;
  colors();
  ensure();

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
  canvas.addEventListener("pointermove", function (e) {
    var r = canvas.getBoundingClientRect();
    pointer.x = e.clientX - r.left - (r.width - size) / 2;
    pointer.y = e.clientY - r.top - (r.height - size) / 2;
    still();
  });
  canvas.addEventListener("pointerleave", function () { pointer.x = pointer.y = -1e4; still(); });
})();
