// mazemail.dev: plays the agent tasks in the HTML Maze window on the home page
// (src/components/MazeWindow.astro). A Maze Magic task (data-kind="magic"): the pointer
// selects a message and clicks Reply, the reply is typed into a draft tab, ⌘M hands it to
// the agent, the improved text appears in a dialog over the draft and the pointer clicks
// Accept & Send. Every other task: the pointer may select a message, the
// prompt is typed into the agent column, the agent's commands run (matching rows light up
// in the list), its answer streams in, the proposal appears at the top of the column, the
// pointer clicks "Accept all" and the mailbox reacts. The mailbox keeps the changes until
// the loop starts over. Pauses off screen, in background tabs and with the Pause button.
// With prefers-reduced-motion each task shows at its proposal, switching every few
// seconds without animation.
(function () {
  var root = document.querySelector("[data-maze-window]");
  if (!root) return;
  var stage = root.parentNode;
  var $ = function (sel, el) {
    return (el || root).querySelector(sel);
  };
  var $$ = function (sel, el) {
    return Array.prototype.slice.call((el || root).querySelectorAll(sel));
  };
  var words = function (s) {
    return s ? s.split(" ") : [];
  };

  // Browsers without CSS trigonometric functions get the scale from here.
  if (!(window.CSS && CSS.supports && CSS.supports("width", "calc(tan(atan2(1px, 1px)) * 1px)"))) {
    var fit = function () {
      root.style.setProperty("--mw-s", String(Math.min(1, stage.clientWidth / root.offsetWidth)));
    };
    fit();
    if ("ResizeObserver" in window) new ResizeObserver(fit).observe(stage);
    else window.addEventListener("resize", fit);
  }

  var tasks = $$(".mw-task");
  var rows = {}, msgs = {};
  $$(".mw-row").forEach(function (r) { rows[r.getAttribute("data-row")] = r; });
  $$(".mw-msg").forEach(function (m) { msgs[m.getAttribute("data-msg")] = m; });
  var list = $(".mw-rows");
  var clip = $(".mw-rowsclip");
  var typed = $("[data-mw-typed]");
  var title = $("[data-mw-title]");
  var ctx = $("[data-mw-ctx]");
  var mode = $("[data-mw-mode]");
  var cursor = $(".mw-cursor");
  var button = document.querySelector("[data-mw-pause]");
  var still = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  var first = $(".mw-row.sel").getAttribute("data-row");
  var selected = first;
  var counts = $$("[data-count]").map(function (el) { return [el, el.textContent]; });
  var texts = $$(".mw-say").map(function (el) { return [el, el.textContent]; });
  var compose = $(".mw-compose");
  var draft = $("[data-mw-draft]");
  var dialog = $(".mw-mdlg");
  var draftText = draft ? draft.textContent : "";

  // ---- A one-timer scheduler that can be paused and resumed where it stopped. ----
  var timer = 0, due = 0, left = 0, next = null;
  var hold = { user: false, view: true, tab: document.hidden };
  function blocked() {
    return hold.user || hold.view || hold.tab;
  }
  function arm() {
    if (blocked() || timer || !next) return;
    due = Date.now() + left;
    timer = setTimeout(function () {
      timer = 0;
      var f = next;
      next = null;
      f();
    }, left);
  }
  function later(ms, f) {
    left = ms;
    next = f;
    arm();
  }
  function update() {
    if (blocked() && timer) {
      clearTimeout(timer);
      timer = 0;
      left = Math.max(0, due - Date.now());
    }
    root.classList.toggle("paused", blocked());
    arm();
  }

  // ---- The mailbox ----
  function select(key) {
    if (!rows[key]) return;
    rows[selected].classList.remove("sel");
    selected = key;
    rows[key].classList.add("sel");
    for (var k in msgs) msgs[k].classList.toggle("cur", k === key);
    ctx.textContent = "@thread " + rows[key].getAttribute("data-subject");
  }
  function clearHits() {
    $$(".mw-row.hit").forEach(function (r) { r.classList.remove("hit"); });
    $$(".mw-msg.sig").forEach(function (m) { m.classList.remove("sig"); });
  }
  // Moves the list so that the given rows are in view, as far as they fit.
  function reveal(keys) {
    var top = Infinity, bottom = 0;
    keys.forEach(function (k) {
      var r = rows[k];
      if (!r || r.classList.contains("gone")) return;
      top = Math.min(top, r.offsetTop);
      bottom = Math.max(bottom, r.offsetTop + r.offsetHeight);
    });
    if (!bottom) return;
    var off = Math.max(0, Math.min(top, bottom - clip.clientHeight));
    list.style.transform = off ? "translateY(" + -off + "px)" : "";
  }
  function bump(el) {
    el.classList.remove("bump");
    void el.offsetWidth;
    el.classList.add("bump");
  }
  // What accepting task `t`'s proposal does.
  function apply(t, animate) {
    words(t.getAttribute("data-gone")).forEach(function (k) { rows[k].classList.add("gone"); });
    words(t.getAttribute("data-flag")).forEach(function (k) { rows[k].classList.add("flag"); });
    var enrich = t.getAttribute("data-enrich");
    if (enrich) msgs[enrich].classList.add("enriched");
    var c = t.getAttribute("data-counts");
    if (c) {
      c = JSON.parse(c);
      Object.keys(c).forEach(function (k) {
        $$('[data-count="' + k + '"]').forEach(function (el) {
          el.textContent = c[k];
          if (animate) bump(el);
        });
        var folder = $('[data-folder="' + k + '"]');
        if (folder && animate) {
          folder.classList.add("drop");
          setTimeout(function () { folder.classList.remove("drop"); }, 1200);
        }
      });
    }
    words(t.getAttribute("data-replied")).forEach(function (k) { rows[k].classList.add("replied"); });
    var after = t.getAttribute("data-after");
    if (after && rows[selected].classList.contains("gone")) select(after);
  }
  // The draft tab and its dialog.
  function composing(on, text, dlg) {
    if (!compose) return;
    root.classList.toggle("composing", on);
    compose.classList.toggle("on", on);
    dialog.classList.toggle("on", !!dlg);
    draft.textContent = text || "";
  }
  function reset() {
    for (var k in rows) rows[k].classList.remove("gone", "flag", "hit", "replied");
    for (var m in msgs) msgs[m].classList.remove("enriched", "sig");
    counts.forEach(function (c) { c[0].textContent = c[1]; });
    $$(".drop").forEach(function (f) { f.classList.remove("drop"); });
    list.style.transform = "";
    composing(false);
    select(first);
  }
  function hitsOf(t) {
    var keys = [];
    $$("[data-hit]", t).forEach(function (el) { keys = keys.concat(words(el.getAttribute("data-hit"))); });
    return keys;
  }

  // ---- The agent column ----
  function show(k, placeholder) {
    var t = tasks[k];
    for (var i = 0; i < tasks.length; i++) {
      tasks[i].classList.toggle("cur", i === k);
      tasks[i].classList.remove("out");
    }
    title.textContent = placeholder ? "New " + t.getAttribute("data-agent") + " Thread" : t.getAttribute("data-title");
    title.classList.toggle("ph", !!placeholder);
    mode.textContent = t.getAttribute("data-mode");
    root.classList.toggle("soon", t.classList.contains("mw-task--soon"));
  }

  // ---- The pointer, in the window's own (unscaled) coordinates ----
  // Elements hidden in the smaller layouts (the message view) aren't pointed at.
  function visible(el) {
    return !!(el && el.getClientRects().length);
  }
  function at(el, fx, fy) {
    var r = root.getBoundingClientRect(), b = el.getBoundingClientRect();
    var s = r.width / root.offsetWidth || 1;
    return [(b.left - r.left + b.width * fx) / s, (b.top - r.top + b.height * fy) / s];
  }
  function point(el, fx, fy) {
    var p = at(el, fx, fy);
    if (!cursor.classList.contains("on")) {
      // Come in from below and to the right of the target.
      cursor.classList.add("jump");
      cursor.style.transform = "translate(" + (p[0] + 70) + "px," + (p[1] + 90) + "px)";
      cursor.getBoundingClientRect();
      cursor.classList.remove("jump");
      cursor.classList.add("on");
    }
    cursor.style.transform = "translate(" + p[0] + "px," + p[1] + "px)";
  }

  // ---- One task as a list of [delay, action] steps ----
  function play(steps, i, done) {
    if (i >= steps.length) return done();
    later(steps[i][0], function () {
      steps[i][1]();
      play(steps, i + 1, done);
    });
  }

  // Streams `el`'s text in word by word.
  function stream(q, el, wait) {
    var text = el.textContent, w = text.split(" ");
    q(wait || 400, function () {
      el.textContent = "";
      el.classList.add("on");
    });
    for (var n = 0; n < w.length; n++)
      (function (n) {
        q(40, function () { el.textContent = w.slice(0, n + 1).join(" "); });
      })(n);
  }

  // Clears task t's steps (and the mailbox, when the loop starts over).
  function begin(t, k, restart) {
    $$(".on, .done, .ok, .press, .gone", t).forEach(function (el) {
      el.classList.remove("on", "done", "ok", "press", "gone");
    });
    typed.textContent = "";
    root.classList.remove("busy");
    clearHits();
    if (restart) {
      reset();
      root.getBoundingClientRect();
      root.classList.remove("resetting");
    }
    composing(false);
    show(k, true);
  }

  function selectSteps(q, key) {
    q(500, function () { reveal([key]); });
    q(250, function () { point(rows[key], 0.42, 0.55); });
    q(900, function () { select(key); });
  }

  function planMagic(k, restart) {
    var t = tasks[k], s = [];
    var q = function (ms, f) { s.push([ms, f]); };
    var key = t.getAttribute("data-select");
    var magic = $("[data-mw-magic]"), accept = $("[data-magic-accept]");
    var reply = function () { return $(".mw-ob", msgs[key]); };

    q(0, function () { begin(t, k, restart); });
    selectSteps(q, key);
    q(700, function () {
      if (visible(reply())) point(reply(), 0.5, 0.55);
    });
    q(650, function () {
      cursor.classList.remove("on");
      composing(true, "");
    });
    for (var c = 1; c <= draftText.length; c++)
      (function (c) {
        q(c === 1 ? 500 : 22 + ((draftText.charCodeAt(c - 1) * 13) % 38), function () {
          draft.textContent = draftText.slice(0, c);
        });
      })(c);
    q(600, function () { point(magic, 0.5, 0.55); });
    q(800, function () { magic.classList.add("press"); });
    q(250, function () {
      magic.classList.remove("press");
      cursor.classList.remove("on");
      root.classList.add("busy");
      show(k, false);
      $(".mw-user", t).classList.add("on");
    });
    q(350, function () { $(".mw-think", t).classList.add("on"); });
    q(1500, function () { $(".mw-think", t).classList.add("done"); });
    stream(q, $(".mw-say:not(.mw-final)", t));
    q(300, function () {
      root.classList.remove("busy");
      dialog.classList.add("on");
    });
    q(1800, function () { point(accept, 0.45, 0.55); });
    q(950, function () { accept.classList.add("press"); });
    q(180, function () { accept.classList.remove("press"); });
    q(350, function () {
      cursor.classList.remove("on");
      composing(false);
      apply(t, true);
      reveal([key]);
    });
    stream(q, $(".mw-final", t), 250);
    q(2800, function () {
      t.classList.add("out");
      if (k === tasks.length - 1) root.classList.add("resetting");
    });
    q(400, function () {});
    return s;
  }

  function plan(k, restart) {
    if (tasks[k].getAttribute("data-kind") === "magic") return planMagic(k, restart);
    var t = tasks[k], s = [];
    var q = function (ms, f) { s.push([ms, f]); };
    var on = function (el) { el.classList.add("on"); };
    var user = $(".mw-user", t), prompt = user.textContent;
    var prop = $(".mw-prop", t), wrap = $(".mw-propwrap", t), accept = $("[data-accept]", t);
    var key = t.getAttribute("data-select");

    q(0, function () { begin(t, k, restart); });
    if (key) {
      selectSteps(q, key);
      q(450, function () { cursor.classList.remove("on"); });
    }
    for (var c = 1; c <= prompt.length; c++)
      (function (c) {
        q(c === 1 ? 600 : 18 + ((prompt.charCodeAt(c - 1) * 13) % 34), function () {
          typed.textContent = prompt.slice(0, c);
        });
      })(c);
    q(450, function () {
      typed.textContent = "";
      root.classList.add("busy");
      show(k, false);
      on(user);
    });
    $$(".mw-conv > .mw-step:not(.mw-user):not(.mw-final)", t).forEach(function (el) {
      if (el.classList.contains("mw-say")) stream(q, el);
      else
        (function (el) {
          q(350, function () { on(el); });
          q(+el.getAttribute("data-dur"), function () {
            el.classList.add("done");
            var hit = words(el.getAttribute("data-hit"));
            hit.forEach(function (h) { rows[h].classList.add("hit"); });
            if (hit.length) reveal(hitsOf(t));
            if (el.hasAttribute("data-sig")) msgs[selected].classList.add("sig");
          });
        })(el);
    });
    q(300, function () { on(prop); });
    q(900, function () { point(accept, 0.4, 0.55); });
    q(950, function () { accept.classList.add("press"); });
    q(180, function () {
      accept.classList.remove("press");
      prop.classList.add("ok");
      root.classList.remove("busy");
    });
    q(650, function () {
      wrap.classList.add("gone");
      cursor.classList.remove("on");
      clearHits();
      apply(t, true);
    });
    stream(q, $(".mw-final", t), 250);
    q(2800, function () {
      t.classList.add("out");
      if (k === tasks.length - 1) root.classList.add("resetting");
    });
    q(400, function () {});
    return s;
  }

  // Reduced motion: task k at its proposal, with the earlier tasks' changes applied.
  function showStill(k) {
    reset();
    for (var i = 0; i < k; i++) {
      if (tasks[i].getAttribute("data-select")) select(tasks[i].getAttribute("data-select"));
      apply(tasks[i], false);
    }
    var t = tasks[k];
    if (t.getAttribute("data-select")) select(t.getAttribute("data-select"));
    hitsOf(t).forEach(function (h) { rows[h].classList.add("hit"); });
    if ($("[data-sig]", t)) msgs[selected].classList.add("sig");
    reveal(hitsOf(t));
    if (t.getAttribute("data-kind") === "magic") composing(true, draftText, true);
    show(k, false);
  }

  function loop(k, restart) {
    var nextK = (k + 1) % tasks.length;
    if (still) {
      showStill(k);
      later(7000, function () { loop(nextK); });
    } else {
      play(plan(k, restart), 0, function () { loop(nextK, nextK === 0); });
    }
  }

  // Restore streamed text first, so a reduced-motion visitor never sees half a sentence.
  texts.forEach(function (x) { x[0].textContent = x[1]; });
  root.classList.add("js");
  if (!still) {
    root.classList.add("live");
    clearHits();
  }
  show(0, false);

  if (button) {
    button.hidden = false;
    button.addEventListener("click", function () {
      hold.user = !hold.user;
      button.textContent = hold.user ? "Play" : "Pause";
      update();
    });
  }
  document.addEventListener("visibilitychange", function () {
    hold.tab = document.hidden;
    update();
  });
  if ("IntersectionObserver" in window) {
    new IntersectionObserver(function (entries) {
      hold.view = !entries[entries.length - 1].isIntersecting;
      update();
    }, { threshold: 0.25 }).observe(stage);
  } else {
    hold.view = false;
  }
  loop(0, false);
})();
