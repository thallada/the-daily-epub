(() => {
  // Marks the document as scripted before first paint so progressively enhanced
  // widgets (the contents panel) can start collapsed without a flash.
  document.documentElement.classList.add("has-js");
  try {
    const theme = localStorage.getItem("theme");
    if (theme === "light" || theme === "dark") document.documentElement.dataset.theme = theme;
    else document.documentElement.removeAttribute("data-theme");
  } catch (_) {
    document.documentElement.removeAttribute("data-theme");
  }
})();
