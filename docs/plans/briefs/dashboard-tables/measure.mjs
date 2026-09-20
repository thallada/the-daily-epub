// usage: node measure.mjs <port> <outdir> [widths]
import { chromium } from "playwright";
import fs from "node:fs";
const [port, outdir, widthsArg] = process.argv.slice(2);
const base = `http://127.0.0.1:${port}`;
const widths = (widthsArg || "390,768,1280,1920,2560").split(",").map(Number);
const pages = ["/dashboard","/dashboard/runs","/dashboard/runs/1","/dashboard/runs/2","/dashboard/articles","/dashboard/articles/1","/dashboard/ratings","/dashboard/ratings?tab=events","/dashboard/interests","/dashboard/feeds","/dashboard/feeds?status=added","/dashboard/profile","/dashboard/stats","/dashboard/jobs","/dashboard/settings","/dashboard/settings/history","/dashboard/users","/","/issues","/account"];
fs.mkdirSync(outdir, { recursive: true });
const browser = await chromium.launch();
const ctx = await browser.newContext({ viewport: { width: 1280, height: 900 } });
const page = await ctx.newPage();
await page.goto(`${base}/login`);
await page.fill('input[name="username"]', "admin");
await page.fill('input[name="password"]', "adminpassword123");
await page.click('button[type="submit"]');
await page.waitForLoadState("networkidle");
const report = [];
for (const w of widths) {
  await page.setViewportSize({ width: w, height: 1000 });
  for (const p of pages) {
    const resp = await page.goto(`${base}${p}`, { waitUntil: "networkidle" });
    const status = resp?.status();
    const info = await page.evaluate(() => {
      const de = document.documentElement;
      const container = document.querySelector(".dashboard, main > section, main > div, main");
      const tables = [...document.querySelectorAll("table")].map((t) => {
        const host = t.closest(".scroll-x") || t.parentElement;
        const r = t.getBoundingClientRect();
        return { cls: t.className, ths: t.querySelectorAll("thead th").length, rows: t.querySelectorAll("tbody tr").length, tableW: Math.round(r.width), hostW: Math.round(host.getBoundingClientRect().width), hostScroll: host.scrollWidth, hostClient: host.clientWidth, overflow: host.scrollWidth > host.clientWidth + 1 };
      });
      const culprits = [...document.querySelectorAll("body *")].filter((el) => el.scrollWidth > el.clientWidth + 1 && getComputedStyle(el).overflowX === "visible" && !el.closest("table")).slice(0, 5).map((el) => `${el.tagName.toLowerCase()}.${String(el.className).split(" ").slice(0,2).join(".")} ${el.scrollWidth}>${el.clientWidth}`);
      return { pageOverflow: de.scrollWidth > de.clientWidth, docW: de.clientWidth, containerW: container ? Math.round(container.getBoundingClientRect().width) : null, tables, culprits };
    });
    const name = p.replace(/[\/?=]/g, "_").replace(/^_/, "") || "root";
    if (w === 1280 || w === 2560 || w === 390) await page.screenshot({ path: `${outdir}/${w}-${name}.png`, fullPage: true });
    report.push({ w, p, status, ...info });
    const bad = info.tables.filter((t) => t.overflow);
    console.log(`${w} ${p} status=${status} page=${info.pageOverflow ? "OVERFLOW" : "ok"} container=${info.containerW} tables=${info.tables.length} scrolling=${bad.length}${bad.length ? " [" + bad.map((t) => `${t.cls || "-"}:${t.hostScroll}>${t.hostClient}`).join(", ") + "]" : ""}${info.culprits.length ? " culprits=" + info.culprits.join(" | ") : ""}`);
  }
}
fs.writeFileSync(`${outdir}/report.json`, JSON.stringify(report, null, 1));
await browser.close();
