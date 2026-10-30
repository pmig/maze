// mazemail.dev: the site-wide script (progressive enhancement, ~4 KB).
// 1. Applies a saved theme before first paint (loaded blocking in <head>).
// 2. Wires the theme toggle. Choosing the theme the system already uses forgets the
//    choice, so the site follows the system again.
// 3. Keyboard shortcuts printed in the header: one letter per link, marked with
//    data-shortcut="<letter>" ("h" goes home, "d" opens the docs). The first link with
//    the letter wins; a second one (the home page's "Read the docs") must go to the same
//    page. Ignored while typing in a field or with cmd/ctrl/alt held, so browser
//    shortcuts keep working.
// 4. Detects the visitor's operating system: sets data-os="mac|linux|windows" on <html>
//    and names their computer in [data-device] elements ("device" stays the default).
// Stored in localStorage under "theme"; no cookies. Nothing is sent anywhere.
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

  // Phones, tablets (iPads in desktop mode report a Mac, so touch counts) and anything
  // unrecognised get no data-os and keep the generic word.
  function os() {
    var ua = navigator.userAgent || "";
    var platform = (navigator.userAgentData && navigator.userAgentData.platform) || navigator.platform || "";
    if (/android|iphone|ipad|ipod|cros/i.test(ua)) return "";
    if (/mac/i.test(platform) || /macintosh/i.test(ua)) return navigator.maxTouchPoints > 0 ? "" : "mac";
    if (/win/i.test(platform) || /windows/i.test(ua)) return "windows";
    if (/linux|x11/i.test(platform) || /linux|x11/i.test(ua)) return "linux";
    return "";
  }
  var DEVICE = { mac: "Mac", linux: "Linux PC", windows: "Windows PC" };
  var visitorOs = os();
  if (visitorOs) root.setAttribute("data-os", visitorOs);

  var initial = saved();
  if (initial === "light" || initial === "dark") root.setAttribute("data-theme", initial);

  document.addEventListener("DOMContentLoaded", function () {
    if (visitorOs) {
      var devices = document.querySelectorAll("[data-device]");
      for (var i = 0; i < devices.length; i++) devices[i].textContent = DEVICE[visitorOs];
    }

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
      if (event.defaultPrevented || event.repeat || event.isComposing) return;
      if (event.metaKey || event.ctrlKey || event.altKey) return;
      var key = (event.key || "").toLowerCase();
      if (!/^[a-z]$/.test(key)) return;
      var target = event.target;
      if (target && (target.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(target.tagName))) return;
      var link = document.querySelector('a[data-shortcut="' + key + '"]');
      if (!link || !link.href) return;
      event.preventDefault();
      // Already there (e.g. "d" on /docs/): stay put instead of reloading.
      if (link.href === window.location.origin + window.location.pathname) return;
      window.location.href = link.href;
    });
  });
})();
