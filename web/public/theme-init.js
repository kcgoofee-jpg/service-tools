// 在 <head> 里同步执行：按用户上次的选择设置主题，避免首屏闪一下亮色。（CSP 只允许同源脚本，所以放成外链小文件。）
try {
  var t = localStorage.getItem("owl-theme");
  if (t === "light" || t === "dark") document.documentElement.setAttribute("data-theme", t);
} catch (e) {}
