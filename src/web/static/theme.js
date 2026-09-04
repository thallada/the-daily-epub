(() => {
  // Marks the document as scripted before first paint so progressively enhanced
  // widgets (the contents panel) can start collapsed without a flash.
  document.documentElement.classList.add("has-js");
  // The color-scheme meta decides what the browser paints before the stylesheet
  // arrives; keep it in step with an explicit theme so that first paint is not
  // the wrong shade.
  const scheme = document.querySelector('meta[name="color-scheme"]');
  try {
    const theme = localStorage.getItem("theme");
    if (theme === "light" || theme === "dark") {
      document.documentElement.dataset.theme = theme;
      if (scheme) scheme.content = theme;
    } else {
      document.documentElement.removeAttribute("data-theme");
    }
  } catch (_) {
    document.documentElement.removeAttribute("data-theme");
  }

  // Restore persisted disclosures as the parser creates them. This head script
  // runs before paint, so a cold app.js request cannot expose the server-default
  // open state and then shift the dashboard when the script finally arrives.
  const restoreDetails = (root) => {
    const details = [];
    if (root.nodeType === Node.ELEMENT_NODE && root.matches("details[id]")) details.push(root);
    root.querySelectorAll?.("details[id]").forEach((element) => details.push(element));
    details.forEach((element) => {
      try {
        element.open = localStorage.getItem("details:" + element.id) === "open";
      } catch (_) {}
    });
  };
  const detailsObserver = new MutationObserver((records) => {
    records.forEach((record) => record.addedNodes.forEach(restoreDetails));
  });
  detailsObserver.observe(document.documentElement, { childList: true, subtree: true });
  document.addEventListener("DOMContentLoaded", () => detailsObserver.disconnect(), { once: true });
})();
