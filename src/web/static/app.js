const themeToggle = document.querySelector("[data-theme-toggle]");
if (themeToggle) {
  const media = window.matchMedia("(prefers-color-scheme: dark)");
  const readTheme = () => {
    try {
      const saved = localStorage.getItem("theme");
      return saved === "light" || saved === "dark" ? saved : "system";
    } catch (_) {
      return "system";
    }
  };
  const renderTheme = (theme) => {
    themeToggle.setAttribute("aria-label", `Theme: ${theme}`);
    themeToggle.querySelector("[data-theme-label]").textContent = theme[0].toUpperCase() + theme.slice(1);
    themeToggle.querySelectorAll("[data-theme-icon]").forEach((icon) => {
      // SVGElement has no `hidden` IDL attribute; toggle the content attribute.
      icon.toggleAttribute("hidden", icon.getAttribute("data-theme-icon") !== theme);
    });
  };
  const setTheme = (theme) => {
    if (theme === "system") document.documentElement.removeAttribute("data-theme");
    else document.documentElement.dataset.theme = theme;
    try {
      if (theme === "system") localStorage.removeItem("theme");
      else localStorage.setItem("theme", theme);
    } catch (_) {}
    renderTheme(theme);
  };
  renderTheme(readTheme());
  themeToggle.addEventListener("click", () => {
    const current = readTheme();
    setTheme(current === "system" ? "light" : current === "light" ? "dark" : "system");
  });
  media.addEventListener("change", () => {
    if (readTheme() === "system") {
      document.documentElement.removeAttribute("data-theme");
      renderTheme("system");
    }
  });
}

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
/* step 5: settings — "reset to default" fills the field with its default */
document.addEventListener("click", (event) => {
  const button = event.target.closest("button[data-reset]");
  if (!button) return;
  const input = document.getElementById(button.dataset.reset);
  if (!input) return;
  input.value = button.dataset.default;
});
/* step 6: reload a job page every N seconds while its job is requested/running */
document.querySelectorAll("[data-refresh]").forEach((element) => {
  const seconds = Number(element.dataset.refresh);
  if (seconds > 0) setTimeout(() => window.location.reload(), seconds * 1000);
});
/* step 4: table-of-contents panel (below `lg`) and reading-progress bar */
const tocPanel = document.querySelector("[data-toc-panel]");
const tocToggle = document.querySelector("[data-toc-toggle]");
if (tocPanel) {
  // Long issues overflow the sidebar; scroll just enough to show where we are.
  const revealCurrent = () => {
    const current = tocPanel.querySelector("a[aria-current=page]");
    if (!current || tocPanel.scrollHeight <= tocPanel.clientHeight) return;
    const margin = 24;
    const top = current.offsetTop - margin;
    const bottom = current.offsetTop + current.offsetHeight + margin;
    if (bottom > tocPanel.scrollTop + tocPanel.clientHeight) {
      tocPanel.scrollTop = bottom - tocPanel.clientHeight;
    } else if (top < tocPanel.scrollTop) {
      tocPanel.scrollTop = Math.max(0, top);
    }
  };
  const setOpen = (open) => {
    tocPanel.dataset.open = open ? "true" : "false";
    if (tocToggle) tocToggle.setAttribute("aria-expanded", open ? "true" : "false");
    if (open) revealCurrent();
  };
  setOpen(false);
  revealCurrent();
  if (tocToggle) {
    tocToggle.addEventListener("click", () => setOpen(tocPanel.dataset.open !== "true"));
    document.addEventListener("click", (event) => {
      if (tocPanel.dataset.open !== "true" || tocToggle.contains(event.target)) return;
      // A tap on a chapter closes the panel; so does a tap anywhere outside it.
      if (!tocPanel.contains(event.target) || event.target.closest("a")) setOpen(false);
    });
    document.addEventListener("keydown", (event) => {
      if (event.key !== "Escape" || tocPanel.dataset.open !== "true") return;
      setOpen(false);
      tocToggle.focus();
    });
    window.matchMedia("(min-width: 64rem)").addEventListener("change", () => setOpen(false));
  }
}
const tocBars = document.querySelectorAll("[data-toc-progress]");
const tocChapter = document.querySelector("[data-toc-scroll]");
if (tocBars.length && tocChapter) {
  // "Chapter N" is worth position N once finished; show N-1 plus how far down we are.
  const base = Math.max(0, Number(tocBars[0].getAttribute("value")) - 1);
  let queued = false;
  const paint = () => {
    queued = false;
    const start = window.scrollY + tocChapter.getBoundingClientRect().top;
    const end = start + tocChapter.offsetHeight - window.innerHeight;
    const read = end > start ? (window.scrollY - start) / (end - start) : 1;
    const value = base + Math.min(1, Math.max(0, read));
    tocBars.forEach((bar) => {
      // The CSS transition is for navigation, not for tracking a finger.
      bar.dataset.live = "true";
      bar.value = value;
    });
  };
  const schedule = () => {
    if (queued) return;
    queued = true;
    window.requestAnimationFrame(paint);
  };
  window.addEventListener("scroll", schedule, { passive: true });
  window.addEventListener("resize", schedule);
  schedule();
}
