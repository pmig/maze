// mazemail.dev: plays the agent demo on the home page (src/components/AgentDemo.astro).
// Each task: the prompt is typed, the agent's tool calls run, its answer streams in, a
// proposal appears and a cursor clicks "Accept all"; then the next task. Pauses off
// screen, in background tabs and with the Pause button. With prefers-reduced-motion the
// tasks show in their final state, switching every few seconds without animation.
(function () {
  var root = document.querySelector("[data-agent-demo]");
  if (!root) return;
  var $ = function (sel, el) {
    return (el || root).querySelector(sel);
  };
  var tasks = root.querySelectorAll(".ag-task");
  var side = root.querySelectorAll(".ag-side-i");
  var stage = $(".ag-stage");
  var typed = $("[data-ag-typed]");
  var cursor = $(".ag-cursor");
  var button = document.querySelector("[data-ag-pause]");
  var still = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  var texts = [];
  for (var i = 0; i < tasks.length; i++) {
    var says = tasks[i].querySelectorAll(".ag-say");
    for (var j = 0; j < says.length; j++) texts.push([says[j], says[j].textContent]);
  }

  // A one-timer scheduler that can be paused and resumed where it stopped.
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

  function show(k) {
    for (var i = 0; i < tasks.length; i++) {
      tasks[i].classList.toggle("cur", i === k);
      tasks[i].classList.remove("out");
      if (side[i]) side[i].classList.toggle("on", i === k);
    }
    $("[data-ag-title]").textContent = tasks[k].getAttribute("data-title");
    $("[data-ag-agent]").textContent = tasks[k].getAttribute("data-agent");
  }

  function play(steps, i, done) {
    if (i >= steps.length) return done();
    later(steps[i][0], function () {
      steps[i][1]();
      stage.scrollTop = stage.scrollHeight;
      play(steps, i + 1, done);
    });
  }

  function plan(k) {
    var t = tasks[k], s = [];
    var q = function (ms, f) {
      s.push([ms, f]);
    };
    var on = function (el) {
      el.classList.add("on");
    };
    q(0, function () {
      var els = t.querySelectorAll(".on, .done, .ok, .press");
      for (var i = 0; i < els.length; i++) els[i].classList.remove("on", "done", "ok", "press");
      typed.textContent = "";
      stage.scrollTop = 0;
      show(k);
    });
    var user = $(".ag-user", t), prompt = user.textContent;
    for (var c = 1; c <= prompt.length; c++)
      (function (c) {
        q(c === 1 ? 700 : 18 + ((prompt.charCodeAt(c - 1) * 13) % 34), function () {
          typed.textContent = prompt.slice(0, c);
        });
      })(c);
    q(500, function () {
      typed.textContent = "";
      root.classList.add("busy");
      on(user);
    });
    var steps = t.querySelectorAll(".ag-step");
    for (var n = 1; n < steps.length; n++)
      (function (el) {
        if (el.classList.contains("ag-row")) {
          q(350, function () { on(el); });
          q(+el.getAttribute("data-dur"), function () { el.classList.add("done"); });
        } else if (el.classList.contains("ag-card")) {
          var accept = $("[data-accept]", el);
          q(300, function () { on(el); });
          q(900, function () {
            // Start below the card, then glide to the button.
            var r = root.getBoundingClientRect(), b = accept.getBoundingClientRect();
            cursor.classList.add("jump");
            cursor.style.transform = "translate(" + (b.left - r.left - 60) + "px," + (r.height - 30) + "px)";
            cursor.getBoundingClientRect();
            cursor.classList.remove("jump");
            cursor.classList.add("on");
            cursor.style.transform = "translate(" + (b.left - r.left + b.width * 0.45) + "px," + (b.top - r.top + b.height * 0.5) + "px)";
          });
          q(950, function () { accept.classList.add("press"); });
          q(180, function () {
            el.classList.add("ok");
            root.classList.remove("busy");
          });
          q(600, function () { cursor.classList.remove("on"); });
        } else {
          var text = el.textContent, words = text.split(" ");
          q(400, function () {
            el.textContent = "";
            on(el);
          });
          for (var w = 0; w < words.length; w++)
            (function (w) {
              q(40, function () { el.textContent = words.slice(0, w + 1).join(" "); });
            })(w);
        }
      })(steps[n]);
    q(2800, function () { t.classList.add("out"); });
    q(400, function () {});
    return s;
  }

  function loop(k) {
    if (still) {
      show(k);
      later(7000, function () { loop((k + 1) % tasks.length); });
    } else {
      play(plan(k), 0, function () { loop((k + 1) % tasks.length); });
    }
  }

  // Restore streamed text first, so a reduced-motion visitor never sees half a sentence.
  for (var x = 0; x < texts.length; x++) texts[x][0].textContent = texts[x][1];
  root.classList.add("js");
  if (!still) root.classList.add("live");
  show(0);

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
    }, { threshold: 0.25 }).observe(root);
  } else {
    hold.view = false;
  }
  loop(0);
})();
