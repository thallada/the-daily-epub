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
/* step 3: filter-as-you-type on tables with data-filter (this page's rows only) */
document.querySelectorAll("table[data-filter]").forEach((table) => {
  const rows = table.querySelectorAll("tbody tr");
  if (rows.length < 2) return;
  const input = document.createElement("input");
  input.type = "search";
  input.className = "table-filter";
  input.placeholder = "Filter rows on this page";
  input.setAttribute("aria-label", "Filter rows on this page");
  const host = table.closest(".scroll-x") || table;
  host.parentNode.insertBefore(input, host);
  input.addEventListener("input", () => {
    const needle = input.value.trim().toLowerCase();
    rows.forEach((row) => {
      row.hidden = needle !== "" && !row.textContent.toLowerCase().includes(needle);
    });
  });
});
/* step 6: reload a job page every N seconds while its job is requested/running */
document.querySelectorAll("[data-refresh]").forEach((element) => {
  const seconds = Number(element.dataset.refresh);
  if (seconds > 0) setTimeout(() => window.location.reload(), seconds * 1000);
});
