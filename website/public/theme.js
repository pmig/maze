// mazemail.dev: the only script on the site (progressive enhancement, ~1 KB).
// 1. Applies a saved theme before first paint (loaded blocking in <head>).
// 2. Wires the theme toggle. Choosing the theme the system already uses forgets the
//    choice, so the site follows the system again.
// 3. Keyboard shortcuts printed in the header: "d" opens Download.
// Stored in localStorage under "theme"; no cookies.
(function () {
  var root = document.documentElement;
  var media = window.matchMedia("(prefers-color-scheme: dark)");

  function saved() {
    try {
      return localStorage.getItem("theme");
    } catch (e) {
      return null;
    }
  }

  function effective() {
    var s = root.getAttribute("data-theme");
    if (s === "light" || s === "dark") return s;
    return media.matches ? "dark" : "light";
  }

  function label(button) {
    var next = effective() === "dark" ? "light" : "dark";
    button.setAttribute("aria-label", "Switch to " + next + " theme");
    button.setAttribute("title", "Switch to " + next + " theme");
  }

  var initial = saved();
  if (initial === "light" || initial === "dark") root.setAttribute("data-theme", initial);

  document.addEventListener("DOMContentLoaded", function () {
    var button = document.getElementById("theme-toggle");
    if (button) {
      button.hidden = false;
      label(button);
      button.addEventListener("click", function () {
        var next = effective() === "dark" ? "light" : "dark";
        var system = media.matches ? "dark" : "light";
        try {
          if (next === system) localStorage.removeItem("theme");
          else localStorage.setItem("theme", next);
        } catch (e) {}
        if (next === system) root.removeAttribute("data-theme");
        else root.setAttribute("data-theme", next);
        label(button);
      });
      media.addEventListener("change", function () {
        label(button);
      });
    }

    document.addEventListener("keydown", function (event) {
      if (event.defaultPrevented || event.metaKey || event.ctrlKey || event.altKey) return;
      var target = event.target;
      if (target && (target.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(target.tagName))) return;
      var link = document.querySelector('[data-shortcut="' + event.key.toLowerCase() + '"]');
      if (link && link.href) {
        event.preventDefault();
        window.location.href = link.href;
      }
    });
  });
})();
