(() => {
  try {
    const theme = localStorage.getItem("theme");
    if (theme === "light" || theme === "dark") document.documentElement.dataset.theme = theme;
    else document.documentElement.removeAttribute("data-theme");
  } catch (_) {
    document.documentElement.removeAttribute("data-theme");
  }
})();
