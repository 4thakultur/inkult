"use strict";

// Loaded synchronously in <head> so the saved theme, starfield and Kult
// toggles and cursor are on <html> before the first paint; app.js is deferred and would flash the
// defaults. The CSP forbids inline scripts, hence a separate file. Keep in
// sync with THEMES, STARS_KEY, KULT_KEY and CURSORS in app.js.
(() => {
  const THEME_COLORS = {
    pipeboy: "#020a03",
    green: "#000000",
    white: "#000000",
    blue: "#000000",
    purple: "#000000",
    orange: "#000000",
    red: "#000000",
    win95: "#008080",
  };
  // Ids the black themes had before they were renamed; a browser that saved
  // one keeps its colour instead of falling back to the default.
  const RENAMED = {
    amoled: "green", "amoled-white": "white", "amoled-blue": "blue",
    "amoled-purple": "purple", "amoled-orange": "orange", "amoled-red": "red",
  };
  let theme = "pipeboy";
  try {
    let saved = localStorage.getItem("inkult.theme");
    if (saved && Object.hasOwn(RENAMED, saved)) {
      saved = RENAMED[saved];
      localStorage.setItem("inkult.theme", saved);
    }
    if (saved && Object.hasOwn(THEME_COLORS, saved)) theme = saved;
  } catch { /* storage unavailable: default theme */ }
  document.documentElement.dataset.theme = theme;
  let stars = "on";
  try {
    if (localStorage.getItem("inkult.stars") === "off") stars = "off";
  } catch { /* storage unavailable: stars on */ }
  document.documentElement.dataset.stars = stars;
  let kult = "on";
  try {
    if (localStorage.getItem("inkult.kult") === "off") kult = "off";
  } catch { /* storage unavailable: Kult on */ }
  document.documentElement.dataset.kult = kult;
  const CURSORS = ["sniper", "basic", "blazing", "soulseek"];
  let cursor = "sniper";
  try {
    const saved = localStorage.getItem("inkult.cursor");
    if (CURSORS.includes(saved)) cursor = saved;
  } catch { /* storage unavailable: default cursor */ }
  document.documentElement.dataset.cursor = cursor;
  const meta = document.querySelector('meta[name="theme-color"]');
  if (meta) meta.content = THEME_COLORS[theme];
})();
