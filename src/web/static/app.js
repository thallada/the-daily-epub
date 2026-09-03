document.addEventListener("submit", (event) => {
  const form = event.target;
  if (form.matches("form.rating") && event.submitter) {
    event.preventDefault();
    const submitted = event.submitter;
    const body = new URLSearchParams(new FormData(form));
    body.set(submitted.name, submitted.value);
    fetch(form.action, {
      method: "POST",
      body,
      credentials: "same-origin",
      headers: { Accept: "application/json" },
    }).then((response) => {
      if (!response.ok) throw new Error("rating request failed");
      return response.json();
    }).then((result) => {
      form.querySelectorAll("button[data-label]").forEach((button) => {
        const active = button.dataset.label === result.label;
        button.classList.toggle("active", active);
        button.setAttribute("aria-pressed", active ? "true" : "false");
      });
    }).catch(() => {
      const label = document.createElement("input");
      label.type = "hidden";
      label.name = submitted.name;
      label.value = submitted.value;
      form.appendChild(label);
      form.submit();
    });
    return;
  }
  const message = form.dataset.confirm;
  if (message && !window.confirm(message)) event.preventDefault();
});
document.querySelectorAll("details[id]").forEach((details) => {
  try {
    const key = "details:" + details.id;
    details.open = localStorage.getItem(key) === "open";
    details.addEventListener("toggle", () => localStorage.setItem(key, details.open ? "open" : "closed"));
  } catch (_) {}
});
