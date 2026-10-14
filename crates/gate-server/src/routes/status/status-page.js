// 公开状态页的主题切换。2.x 是页面里的内联脚本；CSP 收紧后（script-src 'self'）改成同源外链。
// 放在 <head> 里同步执行：先按上次的选择（或系统偏好）设好 data-theme，首屏不闪；按钮在 DOM 就绪后再接线。
// 存储键与首页 / 后台共用 owl-theme（2.x 状态页单独用 owl-status-theme，升级后第一次访问回到跟随系统）。
(function () {
  var KEY = "owl-theme";
  var root = document.documentElement;
  var mq = window.matchMedia ? window.matchMedia("(prefers-color-scheme: dark)") : null;

  function saved() {
    try {
      var t = localStorage.getItem(KEY);
      return t === "light" || t === "dark" ? t : null;
    } catch (e) {
      return null;
    }
  }

  function system() {
    return mq && mq.matches ? "dark" : "light";
  }

  function apply(t) {
    root.setAttribute("data-theme", t);
    var b = document.getElementById("themeBtn");
    if (!b) return;
    b.textContent = t === "dark" ? "☀️" : "🌙";
    b.setAttribute("aria-label", t === "dark" ? "切换到浅色主题" : "切换到深色主题");
  }

  function wire() {
    apply(root.getAttribute("data-theme") || saved() || system());
    var b = document.getElementById("themeBtn");
    if (!b) return;
    b.addEventListener("click", function () {
      var t = root.getAttribute("data-theme") === "dark" ? "light" : "dark";
      try {
        localStorage.setItem(KEY, t);
      } catch (e) {}
      apply(t);
    });
  }

  apply(saved() || system());
  if (mq && mq.addEventListener) {
    mq.addEventListener("change", function (e) {
      if (saved()) return;
      apply(e.matches ? "dark" : "light");
    });
  }
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", wire);
  else wire();
})();
