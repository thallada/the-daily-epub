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
  if (seconds <= 0) return;
  const schedule = () => setTimeout(() => window.location.reload(), seconds * 1000);
  if (document.prerendering) {
    document.addEventListener("prerenderingchange", schedule, { once: true });
  } else {
    schedule();
  }
});
/* step 4: table-of-contents panel, current chapter, and reading progress */
const tocPanel = document.querySelector("[data-toc-panel]");
const tocToggle = document.querySelector("[data-toc-toggle]");
if (tocPanel) {
  const tocBar = document.querySelector("[data-toc-bar]");
  const tocLinks = Array.from(tocPanel.querySelectorAll(".toc-link"));
  const tocBars = document.querySelectorAll("[data-toc-progress]");
  const tocLabel = document.querySelector("[data-toc-label]");
  const tocCount = document.querySelector("[data-toc-count]");
  const tocStatus = document.querySelector("[data-toc-status]");
  const reducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)");
  const largeScreen = window.matchMedia("(min-width: 64rem)");

  // Long issues overflow the sidebar; scroll just enough to show where we are.
  const revealCurrent = () => {
    const current = tocPanel.querySelector(".toc-link[aria-current]");
    if (!current || tocPanel.scrollHeight <= tocPanel.clientHeight) return;
    const stickyHeader = tocPanel.querySelector(":scope > div");
    const topMargin = (stickyHeader && stickyHeader.offsetHeight > 0 ? stickyHeader.offsetHeight : 0) + 24;
    const panelRect = tocPanel.getBoundingClientRect();
    const currentRect = current.getBoundingClientRect();
    const top = tocPanel.scrollTop + currentRect.top - panelRect.top;
    const visibleTop = tocPanel.scrollTop + topMargin;
    const visibleBottom = tocPanel.scrollTop + tocPanel.clientHeight - 24;
    if (top >= visibleTop && top + currentRect.height <= visibleBottom) return;
    const target = Math.max(0, top - Math.max(topMargin, (tocPanel.clientHeight - currentRect.height) / 2));
    tocPanel.scrollTo({ top: target, behavior: reducedMotion.matches ? "auto" : "smooth" });
  };
  const sizeOpenPanel = () => {
    if (tocPanel.dataset.open !== "true" || largeScreen.matches || !tocBar) return;
    tocPanel.style.setProperty("--toc-panel-top", `${Math.max(0, tocBar.getBoundingClientRect().bottom)}px`);
  };
  const setOpen = (open) => {
    tocPanel.dataset.open = open ? "true" : "false";
    document.documentElement.classList.toggle("toc-panel-open", open && !largeScreen.matches);
    if (tocToggle) {
      tocToggle.setAttribute("aria-expanded", open ? "true" : "false");
      tocToggle.setAttribute("aria-label", open ? "Close contents" : "Contents");
    }
    if (open) {
      sizeOpenPanel();
      revealCurrent();
    } else {
      tocPanel.style.removeProperty("--toc-panel-top");
    }
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
    largeScreen.addEventListener("change", () => setOpen(false));
    window.addEventListener("resize", sizeOpenPanel);
    if (window.visualViewport) window.visualViewport.addEventListener("resize", sizeOpenPanel);
  }

  let currentLink = tocPanel.querySelector(".toc-link[aria-current]");
  // Chapters above the current one read as "already passed" in ink rather than ink-2.
  const markPassed = (link) => {
    let passed = true;
    tocLinks.forEach((candidate) => {
      if (candidate === link) passed = false;
      candidate.toggleAttribute("data-passed", passed);
    });
  };
  if (currentLink) markPassed(currentLink);
  const setCurrent = (link) => {
    if (!link || (link === currentLink && link.getAttribute("aria-current") === "location")) return;
    tocLinks.forEach((candidate) => candidate.removeAttribute("aria-current"));
    markPassed(link);
    link.setAttribute("aria-current", "location");
    currentLink = link;
    const position = Math.max(0, Number(link.dataset.tocPosition));
    const total = tocBars.length ? Number(tocBars[0].max) : 0;
    const isEnd = link.hasAttribute("data-toc-end");
    const label = link.querySelector("[data-toc-link-label]");
    if (tocLabel && label) tocLabel.textContent = label.textContent;
    if (tocCount) {
      tocCount.hidden = position === 0 || isEnd;
      tocCount.textContent = `${position} / ${total}`;
    }
    if (tocStatus) tocStatus.textContent = isEnd ? "End of issue" : position > 0 ? `Chapter ${position} of ${total}` : "Front page";
    tocBars.forEach((bar) => {
      delete bar.dataset.live;
      bar.value = position;
    });
    revealCurrent();
  };

  const tocEntries = Array.from(document.querySelectorAll("[data-toc-entry]")).map((entry) => ({
    entry,
    link: tocLinks.find((candidate) => candidate.getAttribute("href") === entry.dataset.tocEntry),
  })).filter(({ link }) => link);
  if (tocEntries.length) {
    let queued = false;
    const paintCurrent = () => {
      queued = false;
      const threshold = (tocBar ? tocBar.offsetHeight : 0) + window.innerHeight / 3;
      let next = tocEntries[0].link;
      tocEntries.forEach(({ entry, link }) => {
        if (entry.getBoundingClientRect().top <= threshold) next = link;
      });
      setCurrent(next);
    };
    const scheduleCurrent = () => {
      if (queued) return;
      queued = true;
      window.requestAnimationFrame(paintCurrent);
    };
    window.addEventListener("scroll", scheduleCurrent, { passive: true });
    window.addEventListener("resize", scheduleCurrent);
    scheduleCurrent();
  }

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
}
