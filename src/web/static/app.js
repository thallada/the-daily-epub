document.addEventListener("submit", (event) => {
  const message = event.target.dataset.confirm;
  if (message && !window.confirm(message)) event.preventDefault();
});
document.querySelectorAll("details[id]").forEach((details) => {
  try {
    const key = "details:" + details.id;
    details.open = localStorage.getItem(key) === "open";
    details.addEventListener("toggle", () => localStorage.setItem(key, details.open ? "open" : "closed"));
  } catch (_) {}
});
