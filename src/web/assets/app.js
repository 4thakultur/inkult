"use strict";

// All provider data is untrusted: it is only ever inserted via textContent
// or validated https image URLs, never as HTML.

// Buttons are text, BBS style; brackets come from CSS.
const GLYPHS = {
  download: "GET", check: "OK", close: "KILL", save: "SAVE", clock: "WAIT", convert: "CONV", share: "SEND", remove: "DEL",
  play: ">", pause: "||", volumeHigh: "VOL", volumeLow: "VOL", volumeOff: "MUTE",
};
const STATE_LABELS = {
  queued: "Queued", resolving: "Resolving", downloading: "Downloading",
  processing: "Tagging", organizing: "Filing", completed: "Done",
  skipped: "On disk", failed: "Failed", cancelled: "Cancelled",
};
const TERMINAL = new Set(["completed", "skipped", "failed", "cancelled"]);
const PROVIDER_NAMES = { youtube: "YouTube", soundcloud: "SoundCloud", soulseek: "Soulseek" };
// Output formats offered when downloading. Only MP3 and AIFF re-encode;
// the descriptions say so plainly because re-encoding never adds quality.
const OUTPUTS = [
  { id: "original", label: "Best quality", detail: "Original file as delivered (usually Opus or AAC). No re-encoding." },
  { id: "aac", label: "AAC (M4A)", detail: "No re-encoding, plays almost everywhere. Lower bitrate than Opus on YouTube (~128 kbps)." },
  { id: "mp3_320", label: "MP3 320 kbps", detail: "Re-encoded for maximum compatibility. Bigger file, but not better sound than the source." },
  { id: "aiff", label: "AIFF", detail: "Uncompressed, for CDJs and Rekordbox. No extra loss; about 10 MB per minute." },
];
const OUTPUT_SHORT = { original: "Original", aac: "AAC", mp3_320: "MP3 320", aiff: "AIFF", lossless: "Lossless" };
const OUTPUT_KEY = "inkult.output";
const VOLUME_KEY = "inkult.volume";
/** Whether SoundCloud artist pages also list the tracks the artist reposted. */
const REPOSTS_KEY = "inkult.artistReposts";
const LIBRARY_SORT_KEY = "inkult.librarySort";
// UI colour themes; `color` feeds <meta name="theme-color">. theme.js applies
// the saved one before first paint and must list the same ids.
const THEMES = {
  pipeboy: { color: "#020a03", detail: "Green phosphor on a green-tinted black. The original look." },
  amoled: { color: "#000000", detail: "Pure black background for OLED screens. Same phosphor text, no tint." },
  "amoled-white": { color: "#000000", detail: "Pure black background, monochrome white phosphor." },
  "amoled-blue": { color: "#000000", detail: "Pure black background, cyan-blue phosphor." },
  "amoled-purple": { color: "#000000", detail: "Pure black background, violet phosphor." },
  "amoled-orange": { color: "#000000", detail: "Pure black background, amber phosphor like an old amber monitor. Errors turn crimson." },
  "amoled-red": { color: "#000000", detail: "Pure black background, red phosphor. Errors turn hazard yellow, warnings amber." },
};
const DEFAULT_THEME = "pipeboy";
const THEME_KEY = "inkult.theme";
// Starfield in the side margins: "on" (default) or "off"; theme.js applies it.
const STARS_KEY = "inkult.stars";
// Kult, the dancing skeleton mascot: "on" (default) or "off"; theme.js applies it.
const KULT_KEY = "inkult.kult";
// Mouse cursors (desktop pointers only; touch screens have none). The
// images live in app.css under html[data-cursor]; theme.js applies the
// saved one and must list the same ids.
const CURSORS = {
  basic: "Your system's normal arrow and hand pointers.",
  sniper: "Sniper scope. Turns red and tightens over anything clickable.",
  blazing: "A pixel arrow in the theme colour, on fire in a matching hue. Burns white-hot over anything clickable.",
  soulseek: "The Soulseek bird. Turns green over anything clickable.",
};
const DEFAULT_CURSOR = "sniper";
const CURSOR_KEY = "inkult.cursor";

const state = {
  /** The logged-in account ({ id, username, role }), or null. */
  user: null,
  /** Whether the app UI has been started once in this page. */
  started: false,
  view: "search",
  libraryScope: "mine",
  jobsScope: "mine",
  crewRole: "pirate",
  provider: "all",
  /** Query string of the last provider search, if the results show one. */
  lastSearch: null,
  /** Bumped whenever the results area starts showing something new, so a
   *  slow answer to an older request does not replace it. */
  resultsSeq: 0,
  /** Re-renders the last search or link results (from an artist page). */
  backToResults: null,
  jobs: [],
  library: [],
  /** Track identity → output format picked in its library format menu. */
  libraryFormat: new Map(),
  /** Library order: a LIBRARY_SORTS key and whether it is reversed. */
  librarySort: { by: "added", reversed: false },
  /** Track identities ticked for bulk actions; each acts on its picked format. */
  librarySelected: new Set(),
  /** Last ticked track, where a shift-click range starts. */
  libraryAnchor: null,
  /** Identities of the tracks listed right now, in list order. */
  libraryShown: [],
  /** Whether a bulk action is running (its buttons are then disabled). */
  libraryBusy: false,
  jobEls: new Map(),
  /** Job id → state at the last poll, for announcing transitions. */
  jobStates: null,
  pollTimer: null,
};

// ---------- helpers ----------

const $ = (selector) => document.querySelector(selector);

// Wraps a search-bar input so a placeholder wider than the bar scrolls left and
// right instead of being cut off. It only moves while it overflows, which is
// re-checked whenever the bar changes width.
function marqueePlaceholder(input) {
  const text = h("span", {}, input.placeholder);
  const overlay = h("span", { class: "ph-marquee", "aria-hidden": "true" }, text);
  const wrap = h("span", { class: "ph-wrap" });
  input.replaceWith(wrap);
  wrap.append(input, overlay);
  const measure = () => {
    const shift = text.scrollWidth - wrap.clientWidth;
    wrap.classList.toggle("overflows", shift > 0);
    if (shift > 0) {
      wrap.style.setProperty("--ph-shift", `${-shift}px`);
      // ~25px/s of travel, plus the pauses held at each end.
      wrap.style.setProperty("--ph-time", `${Math.max(3, shift / 25 / 0.6).toFixed(1)}s`);
    }
  };
  new ResizeObserver(measure).observe(wrap);
  document.fonts?.ready.then(measure);
}

function h(tag, attrs, ...children) {
  const el = document.createElement(tag);
  for (const [key, value] of Object.entries(attrs || {})) {
    if (value == null || value === false) continue;
    if (key === "class") el.className = value;
    else if (key.startsWith("on")) el.addEventListener(key.slice(2), value);
    else el.setAttribute(key, value === true ? "" : String(value));
  }
  for (const child of children.flat()) {
    if (child == null || child === false) continue;
    el.append(child instanceof Node ? child : String(child));
  }
  return el;
}

function icon(name) {
  return h("span", { "aria-hidden": "true" }, GLYPHS[name]);
}

function art(url, cls = "art") {
  const img = h("img", { class: cls, alt: "", loading: "lazy", referrerpolicy: "no-referrer" });
  if (typeof url === "string" && url.startsWith("https://")) img.src = url;
  return img;
}

function formatDuration(secs) {
  if (typeof secs !== "number" || !isFinite(secs)) return "";
  const s = Math.round(secs);
  const hours = Math.floor(s / 3600);
  const mins = Math.floor((s % 3600) / 60);
  const rest = String(s % 60).padStart(2, "0");
  return hours ? `${hours}:${String(mins).padStart(2, "0")}:${rest}` : `${mins}:${rest}`;
}

function formatTime(secs) {
  return typeof secs === "number" && isFinite(secs) && secs >= 0 ? formatDuration(Math.floor(secs)) : "--:--";
}

function formatBytes(n) {
  if (typeof n !== "number") return "?";
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(0)} KB`;
  if (n < 1024 ** 3) return `${(n / 1024 / 1024).toFixed(1)} MB`;
  return `${(n / 1024 ** 3).toFixed(2)} GB`;
}

/** "44.1 kHz" and "320 kbps" never break between number and unit. */
function keepUnits(text) {
  return text.replace(/(\d) (?=k?Hz|kbps|bit)/gi, "$1\u00a0").replace(/(\d)-(?=bit)/gi, "$1\u2011");
}

function providerTag(provider) {
  return h("span", { class: `provider-tag ${provider}` }, PROVIDER_NAMES[provider] || provider);
}

let toastTimer = null;
function toast(message, isError = false) {
  const el = $("#toast");
  el.textContent = message;
  el.classList.toggle("error", isError);
  el.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { el.hidden = true; }, isError ? 6000 : 2500);
}

async function api(path, { method = "GET", body, signal } = {}) {
  const init = { method, headers: {}, signal };
  if (body !== undefined) {
    init.headers["Content-Type"] = "application/json";
    init.body = JSON.stringify(body);
  }
  const res = await fetch(path, init);
  const text = await res.text();
  let data = null;
  if (text) {
    try { data = JSON.parse(text); } catch { data = null; }
  }
  // The session expired or was revoked (password change, account deleted).
  if (res.status === 401 && data && data.login_required) sessionLost();
  if (!res.ok) {
    const err = new Error((data && data.error) || `Request failed (${res.status})`);
    err.status = res.status;
    throw err;
  }
  return data;
}

const isCaptain = () => Boolean(state.user) && state.user.role === "captain";

// ---------- navigation ----------

function showView(view) {
  if (view === "admin" && !isCaptain()) view = "search";
  state.view = view;
  for (const btn of document.querySelectorAll(".nav-item")) {
    btn.classList.toggle("active", btn.dataset.view === view);
  }
  for (const section of document.querySelectorAll(".view")) {
    section.hidden = section.id !== `view-${view}`;
  }
  // The chat is laid out to fit the viewport exactly (see .chat-mode in app.css).
  document.body.classList.toggle("chat-mode", view === "messages");
  $("#appbar-title").textContent = VIEW_TITLES[view] || view.toUpperCase();
  if (COMPACT.matches) {
    // Picked from the drawer: land at the top of the new view, not halfway
    // down wherever the previous one was scrolled to.
    closePanel({ restoreFocus: false });
    window.scrollTo(0, 0);
  }
  if (view === "messages") {
    window.scrollTo(0, 0);
    document.querySelector(".nav-item.active")?.scrollIntoView({ block: "nearest", inline: "nearest" });
  }
  // Filter fields can change without an input event (browser restore,
  // password managers), so recount them whenever search is shown.
  if (view === "search") syncFiltersToggle();
  if (view === "library") loadLibrary();
  if (view === "downloads") renderJobs();
  if (view === "admin") {
    loadRules();
    loadCrew();
  }
  if (view === "settings") {
    renderSettings();
    loadTwoFactor();
  }
  if (view === "soulseek") slskEnter();
  if (view === "messages") roomsEnter();
  schedulePoll(0);
}

// ---------- compact layout: side panels ----------

// Phones and short landscape screens. Must match the "compact" media
// queries in app.css: there the header, logo and menu live in a panel that
// slides in from the left, the chat rooms in one from the right, and a slim
// top bar opens them.
const COMPACT = window.matchMedia("(max-width: 640px), (max-height: 560px)");
const VIEW_TITLES = {
  search: "SEARCH", soulseek: "SOULSEEK", downloads: "DOWNLOADS", library: "LIBRARY",
  messages: "MESSAGES", settings: "SETTINGS", admin: "ADMIN",
};

/**
 * The slide-in panels. `backdrop` lists what becomes inert while the panel
 * is open, so focus and taps stay in it; the rooms panel lives inside the
 * main box, so only its neighbours there are listed.
 */
const PANELS = {
  menu: {
    panel: "#drawer", opener: "#drawer-open", side: "left",
    focus: () => document.querySelector(".nav-item.active") || $("#drawer-close"),
    backdrop: () => [$("#appbar"), $("#banner"), $(".main"), $(".tagline"), $("#player")],
  },
  rooms: {
    panel: "#irc-side", opener: "#rooms-open", side: "right",
    focus: () => document.querySelector("#irc-side .irc-room.active") || $("#rooms-close"),
    backdrop: () => [$("#appbar"), $("#banner"), $("#room-main"), $(".tagline"), $("#player")],
  },
};
let openPanelName = null;

function openPanel(name) {
  if (!COMPACT.matches || openPanelName === name) return;
  closePanel({ restoreFocus: false });
  const spec = PANELS[name];
  openPanelName = name;
  $(spec.panel).classList.add("open");
  document.body.classList.add("panel-open");
  $(spec.opener).setAttribute("aria-expanded", "true");
  for (const el of spec.backdrop()) el.inert = true;
  spec.focus().focus({ preventScroll: true });
}

function closePanel({ restoreFocus = true } = {}) {
  if (!openPanelName) return;
  const spec = PANELS[openPanelName];
  openPanelName = null;
  $(spec.panel).classList.remove("open");
  document.body.classList.remove("panel-open");
  $(spec.opener).setAttribute("aria-expanded", "false");
  for (const el of spec.backdrop()) el.inert = false;
  if (restoreFocus) $(spec.opener).focus({ preventScroll: true });
}

/** A panel follows a swipe back towards its edge and closes past a threshold. */
function setupPanelSwipe(panel, side) {
  const toEdge = side === "left" ? -1 : 1;
  let start = null;
  let dx = 0;
  panel.addEventListener("touchstart", (event) => {
    if (event.touches.length !== 1) return;
    start = { x: event.touches[0].clientX, y: event.touches[0].clientY, horizontal: null };
    dx = 0;
  }, { passive: true });
  panel.addEventListener("touchmove", (event) => {
    if (!start) return;
    const x = event.touches[0].clientX - start.x;
    const y = event.touches[0].clientY - start.y;
    // Decide once per gesture whether it is a swipe or a scroll of the panel.
    if (start.horizontal === null && Math.hypot(x, y) > 8) start.horizontal = Math.abs(x) > Math.abs(y);
    if (!start.horizontal) return;
    dx = toEdge * Math.max(0, toEdge * x);
    panel.classList.add("dragging");
    panel.style.transform = `translateX(${dx}px)`;
  }, { passive: true });
  const end = () => {
    if (!start) return;
    const swiped = start.horizontal && Math.abs(dx) > Math.min(80, panel.offsetWidth / 3);
    start = null;
    panel.classList.remove("dragging");
    panel.style.removeProperty("transform");
    if (swiped) closePanel();
  };
  panel.addEventListener("touchend", end);
  panel.addEventListener("touchcancel", end);
}

/** Dots on the top bar buttons while a menu entry has a badge (unread, active). */
function syncAppbarDot() {
  const nav = $(".menubar");
  $("#appbar-dot").hidden = !Array.from(nav.querySelectorAll(".badge")).some((b) => !b.hidden);
  $("#rooms-dot").hidden = $("#rooms-badge").hidden;
}

function setupPanels() {
  $("#drawer-open").addEventListener("click", () => openPanel("menu"));
  $("#drawer-close").addEventListener("click", () => closePanel());
  $("#rooms-open").addEventListener("click", () => openPanel("rooms"));
  $("#rooms-close").addEventListener("click", () => closePanel());
  $("#scrim").addEventListener("click", () => closePanel());
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && openPanelName) {
      event.preventDefault();
      closePanel();
    }
  });
  // Rotating to a wide layout puts the panels back in the page.
  COMPACT.addEventListener("change", () => { if (!COMPACT.matches) closePanel({ restoreFocus: false }); });
  for (const spec of Object.values(PANELS)) setupPanelSwipe($(spec.panel), spec.side);
  new MutationObserver(syncAppbarDot)
    .observe($(".menubar"), { subtree: true, attributes: true, attributeFilter: ["hidden"] });
  syncAppbarDot();
}

// ---------- settings ----------

function currentTheme() {
  const theme = document.documentElement.dataset.theme;
  return Object.hasOwn(THEMES, theme) ? theme : DEFAULT_THEME;
}

function applyTheme(theme) {
  if (!Object.hasOwn(THEMES, theme)) return;
  document.documentElement.dataset.theme = theme;
  const meta = document.querySelector('meta[name="theme-color"]');
  if (meta) meta.content = THEMES[theme].color;
  try { localStorage.setItem(THEME_KEY, theme); } catch { /* storage unavailable */ }
}

function starsEnabled() {
  return document.documentElement.dataset.stars !== "off";
}

function applyStars(enabled) {
  const value = enabled ? "on" : "off";
  document.documentElement.dataset.stars = value;
  try { localStorage.setItem(STARS_KEY, value); } catch { /* storage unavailable */ }
}

function kultEnabled() {
  return document.documentElement.dataset.kult !== "off";
}

function applyKult(enabled) {
  const value = enabled ? "on" : "off";
  document.documentElement.dataset.kult = value;
  try { localStorage.setItem(KULT_KEY, value); } catch { /* storage unavailable */ }
  syncKultRoutine();
}

function currentCursor() {
  const cursor = document.documentElement.dataset.cursor;
  return Object.hasOwn(CURSORS, cursor) ? cursor : DEFAULT_CURSOR;
}

function applyCursor(cursor) {
  if (!Object.hasOwn(CURSORS, cursor)) return;
  document.documentElement.dataset.cursor = cursor;
  try { localStorage.setItem(CURSOR_KEY, cursor); } catch { /* storage unavailable */ }
}

// Blazing is animated, which CSS cursors can't be, so a fixed
// overlay (#cursor-fx) follows the mouse instead. CSS stays the source of
// truth for what is clickable: with <html data-cursor-fx> set, idle areas
// compute to `cursor: none` and clickable ones to an invisible image
// tagged "fx-hot" (see app.css). Any other computed cursor (a text field's
// I-beam, a modal dialog) is shown natively and the overlay hides.
const FX_CURSORS = new Set(["blazing"]);
const FINE_POINTER = "(hover: hover) and (pointer: fine)";
const cursorFx = { x: 0, y: 0, target: null, frame: 0 };

function syncCursorFx() {
  const on = matchMedia(FINE_POINTER).matches && FX_CURSORS.has(currentCursor());
  document.documentElement.toggleAttribute("data-cursor-fx", on);
  scheduleCursorFx();
}

function scheduleCursorFx() {
  if (!cursorFx.frame) cursorFx.frame = requestAnimationFrame(drawCursorFx);
}

function drawCursorFx() {
  cursorFx.frame = 0;
  const el = $("#cursor-fx");
  let fxState = null;
  if (cursorFx.target instanceof Element && document.documentElement.hasAttribute("data-cursor-fx")) {
    const cursor = getComputedStyle(cursorFx.target).cursor;
    if (cursor === "none") fxState = "idle";
    else if (cursor.includes("fx-hot")) fxState = "hot";
  }
  el.hidden = !fxState;
  if (!fxState) return;
  el.dataset.state = fxState;
  el.style.transform = `translate(${cursorFx.x}px, ${cursorFx.y}px)`;
}

function setupCursorFx() {
  document.addEventListener("pointermove", (event) => {
    if (event.pointerType !== "mouse") return;
    cursorFx.x = event.clientX;
    cursorFx.y = event.clientY;
    cursorFx.target = event.target;
    scheduleCursorFx();
  }, { passive: true });
  // Leaving the window: hide rather than freeze at the edge.
  document.addEventListener("mouseout", (event) => {
    if (event.relatedTarget) return;
    cursorFx.target = null;
    scheduleCursorFx();
  });
  matchMedia(FINE_POINTER).addEventListener("change", syncCursorFx);
  syncCursorFx();
}

// ---------- scrollbars ----------
// On desktop the native scrollbars are swapped for drawn ones. While a
// native bar is dragged the browser keeps the mouse to itself (no pointer
// events reach the page) and shows the OS arrow over it whatever the CSS
// says, which broke every custom cursor. Scrolling itself stays native
// (wheel, keys, touch); only the bar is ours. Inner scrollers paint theirs
// as a background in a padding gutter (.sb-on in app.css); the page gets
// the fixed #page-scrollbar. Scrollers in dialogs and menus keep native
// bars, since modals fall back to the system cursors anyway.
const SCROLLERS = ".irc-log, .slsk-log, .slsk-desc, .irc-rooms, .irc-users, .drawer, .irc-side";
const SB_WIDTH = 10;
const SB_MIN_THUMB = 24;
// Native bars page by a bit less than a screen when the track is clicked,
// first after a pause, then repeating while the button is held.
const SB_PAGE = 0.875;
const SB_REPEAT_DELAY = 300;
const SB_REPEAT_EVERY = 50;
const sb = { frame: 0, drag: null, page: null, hot: null };

function scheduleScrollbars() {
  if (!sb.frame) sb.frame = requestAnimationFrame(refreshScrollbars);
}

/** The element that scrolls for a bar: a SCROLLERS match, or the page. */
function sbScroller(owner) {
  return owner === "page" ? document.scrollingElement : owner;
}

/** Track and thumb geometry for a bar, in viewport pixels; null when there
 *  is nothing to scroll. */
function sbGeometry(owner) {
  const el = sbScroller(owner);
  const top = owner === "page" ? 0 : el.getBoundingClientRect().top + el.clientTop;
  const track = el.clientHeight;
  const range = el.scrollHeight - el.clientHeight;
  if (range < 1 || track <= 0) return null;
  const height = Math.min(track, Math.max(SB_MIN_THUMB, track * el.clientHeight / el.scrollHeight));
  const y = (track - height) * Math.min(1, Math.max(0, el.scrollTop / range));
  return { top, track, range, height, y };
}

function setStyleVar(el, name, value) {
  if (el.style.getPropertyValue(name) !== value) el.style.setProperty(name, value);
}

function paintScrollbar(el, geometry, owner) {
  if (geometry) {
    setStyleVar(el, "--sb-y", `${geometry.y}px`);
    setStyleVar(el, "--sb-h", `${geometry.height}px`);
  }
  el.classList.toggle("sb-hot", sb.hot === owner);
  el.classList.toggle("sb-drag", sb.drag?.owner === owner);
}

function refreshScrollbars() {
  sb.frame = 0;
  const on = document.documentElement.hasAttribute("data-scrollbars");
  for (const el of document.querySelectorAll(SCROLLERS)) {
    el.classList.toggle("sb", on);
    const geometry = on ? sbGeometry(el) : null;
    el.classList.toggle("sb-on", Boolean(geometry));
    paintScrollbar(el, geometry, el);
  }
  const bar = $("#page-scrollbar");
  const geometry = on ? sbGeometry("page") : null;
  if (bar.hidden === Boolean(geometry)) bar.hidden = !geometry;
  document.documentElement.toggleAttribute("data-page-scroll", Boolean(geometry));
  paintScrollbar(bar, geometry, "page");
}

/** The drawn bar under the pointer, if any: its owner (a scroller or
 *  "page"), geometry and the pointer's offset along the track. */
function scrollbarAt(event) {
  const target = event.target;
  if (!(target instanceof Element)) return null;
  let owner = null;
  if (target.closest("#page-scrollbar")) {
    owner = "page";
  } else if (target.classList.contains("sb-on")) {
    // The gutter is the scroller's own padding, so the scroller itself is
    // the target there, never one of its children.
    const rect = target.getBoundingClientRect();
    const right = rect.left + target.clientLeft + target.clientWidth;
    if (event.clientX >= right - SB_WIDTH && event.clientX < right) owner = target;
  }
  const geometry = owner && sbGeometry(owner);
  if (!geometry) return null;
  const at = event.clientY - geometry.top;
  return { owner, geometry, at, onThumb: at >= geometry.y && at < geometry.y + geometry.height };
}

function scrollOwnerTo(owner, top) {
  sbScroller(owner).scrollTo({ top, behavior: "instant" });
}

// One page towards the pointer, unless the thumb already reached it.
function pageTowardsPointer() {
  const page = sb.page;
  if (!page) return false;
  const geometry = sbGeometry(page.owner);
  if (!geometry) return false;
  const at = page.y - geometry.top;
  const direction = at < geometry.y ? -1 : at >= geometry.y + geometry.height ? 1 : 0;
  if (direction !== page.direction) return false;
  const el = sbScroller(page.owner);
  scrollOwnerTo(page.owner, el.scrollTop + direction * el.clientHeight * SB_PAGE);
  return true;
}

function stopPaging() {
  if (sb.page) clearTimeout(sb.page.timer);
  sb.page = null;
}

function repeatPaging() {
  if (!sb.page) return;
  sb.page.timer = setTimeout(() => {
    if (pageTowardsPointer()) repeatPaging();
    else stopPaging();
  }, SB_REPEAT_EVERY);
}

function setupScrollbars() {
  const fine = matchMedia(FINE_POINTER);
  const sync = () => {
    document.documentElement.toggleAttribute("data-scrollbars", fine.matches);
    scheduleScrollbars();
  };
  fine.addEventListener("change", sync);

  document.addEventListener("pointerdown", (event) => {
    if (event.pointerType === "touch" || event.button !== 0) return;
    const hit = scrollbarAt(event);
    if (!hit) return;
    // No focus change or text selection, as with a native bar.
    event.preventDefault();
    event.target.setPointerCapture(event.pointerId);
    if (hit.onThumb) {
      const el = sbScroller(hit.owner);
      sb.drag = {
        owner: hit.owner, pointerId: event.pointerId, startY: event.clientY, startTop: el.scrollTop,
        perPixel: hit.geometry.range / Math.max(1, hit.geometry.track - hit.geometry.height),
      };
    } else {
      stopPaging();
      sb.page = {
        owner: hit.owner, pointerId: event.pointerId, y: event.clientY,
        direction: hit.at < hit.geometry.y ? -1 : 1, timer: 0,
      };
      pageTowardsPointer();
      if (sb.page) sb.page.timer = setTimeout(repeatPaging, SB_REPEAT_DELAY);
    }
    scheduleScrollbars();
  }, true);

  document.addEventListener("pointermove", (event) => {
    if (sb.drag?.pointerId === event.pointerId) {
      const { owner, startY, startTop, perPixel } = sb.drag;
      scrollOwnerTo(owner, startTop + (event.clientY - startY) * perPixel);
      return;
    }
    if (sb.page?.pointerId === event.pointerId) sb.page.y = event.clientY;
    if (event.pointerType === "touch") return;
    const hit = scrollbarAt(event);
    const hot = hit?.onThumb ? hit.owner : null;
    if (hot !== sb.hot) {
      sb.hot = hot;
      scheduleScrollbars();
    }
  }, { passive: true });

  const release = (event) => {
    if (sb.page?.pointerId === event.pointerId) stopPaging();
    if (sb.drag?.pointerId !== event.pointerId) return;
    sb.drag = null;
    scheduleScrollbars();
  };
  document.addEventListener("pointerup", release, true);
  document.addEventListener("pointercancel", release, true);
  document.addEventListener("lostpointercapture", release, true);

  // Anything that can move a thumb or change what overflows.
  document.addEventListener("scroll", scheduleScrollbars, { capture: true, passive: true });
  document.addEventListener("load", scheduleScrollbars, true);
  window.addEventListener("resize", scheduleScrollbars);
  document.fonts?.ready.then(scheduleScrollbars);
  new MutationObserver(scheduleScrollbars).observe(document.body, {
    childList: true, subtree: true, characterData: true, attributes: true, attributeFilter: ["class", "hidden", "open"],
  });
  sync();
}

function renderSettings() {
  const theme = currentTheme();
  for (const chip of document.querySelectorAll("#theme-options .chip")) {
    const active = chip.dataset.themeId === theme;
    chip.classList.toggle("active", active);
    chip.setAttribute("aria-checked", String(active));
  }
  $("#theme-detail").textContent = THEMES[theme].detail;
  const stars = starsEnabled() ? "on" : "off";
  for (const chip of document.querySelectorAll("#stars-options .chip")) {
    const active = chip.dataset.stars === stars;
    chip.classList.toggle("active", active);
    chip.setAttribute("aria-checked", String(active));
  }
  const kult = kultEnabled() ? "on" : "off";
  for (const chip of document.querySelectorAll("#kult-options .chip")) {
    const active = chip.dataset.kult === kult;
    chip.classList.toggle("active", active);
    chip.setAttribute("aria-checked", String(active));
  }
  const cursor = currentCursor();
  for (const chip of document.querySelectorAll("#cursor-options .chip")) {
    const active = chip.dataset.cursorId === cursor;
    chip.classList.toggle("active", active);
    chip.setAttribute("aria-checked", String(active));
  }
  $("#cursor-detail").textContent = CURSORS[cursor];
}

// ---------- status ----------

async function loadStatus() {
  try {
    const status = await api("/api/status");
    $("#version").textContent = status.version;
    // Only captains are told where the library lives on the server.
    const dir = $("#library-dir");
    dir.textContent = status.library_dir || "/MUSIC/";
    dir.classList.toggle("keep-case", Boolean(status.library_dir));
    const missing = status.tools.filter((tool) => !tool.available);
    const banner = $("#banner");
    banner.hidden = missing.length === 0;
    banner.textContent = missing.length
      ? `Missing required tools: ${missing.map((t) => t.name).join(", ")}. ` +
        "Searching and downloading will not work until they are installed " +
        "(see README) or configured via INKULT_YTDLP / INKULT_FFMPEG."
      : "";
  } catch (err) {
    toast(`Cannot reach Inkult: ${err.message}`, true);
  }
}

// ---------- format menu ----------

function preferredOutput() {
  try {
    const value = localStorage.getItem(OUTPUT_KEY);
    return OUTPUTS.some((o) => o.id === value) ? value : "original";
  } catch {
    return "original";
  }
}

function rememberOutput(id) {
  try { localStorage.setItem(OUTPUT_KEY, id); } catch { /* storage unavailable */ }
}

let openMenu = null;

function closeFormatMenu() {
  if (!openMenu) return;
  const { menu, anchor, onDocPointer, onKey } = openMenu;
  openMenu = null;
  menu.remove();
  document.removeEventListener("pointerdown", onDocPointer, true);
  document.removeEventListener("keydown", onKey, true);
  window.removeEventListener("resize", closeFormatMenu);
  window.removeEventListener("scroll", closeFormatMenu);
  anchor.setAttribute("aria-expanded", "false");
}

const PHONE = window.matchMedia("(max-width: 640px)");

/**
 * Show a format menu next to `anchor`; `onPick(id)` on choice. Without
 * `choices` it is the download menu (all formats, remembers the pick).
 * `choices`: [{ id, label, detail, disabled }] for other uses, such as
 * converting, where impossible formats are shown disabled with the reason.
 * `extras`: actions listed after the formats of the download menu (never
 * remembered as the preferred format).
 */
function openFormatMenu(anchor, onPick, { title = "SELECT FORMAT:", choices = null, extras = [] } = {}) {
  const wasOpenHere = openMenu && openMenu.anchor === anchor;
  closeFormatMenu();
  if (wasOpenHere) return;

  const preferred = choices ? null : preferredOutput();
  const options = [...(choices || OUTPUTS), ...extras];
  const isFormat = (option) => !choices && !extras.includes(option);
  const buttons = options.map((output) => h("button", {
    type: "button", role: isFormat(output) ? "menuitemradio" : "menuitem",
    "aria-checked": isFormat(output) ? String(output.id === preferred) : null,
    "aria-disabled": output.disabled ? "true" : null,
    class: ["menu-item", isFormat(output) && output.id === preferred && "selected",
      output.disabled && "disabled", extras.includes(output) && "menu-extra"].filter(Boolean).join(" "),
    onclick: () => {
      if (output.disabled) return;
      closeFormatMenu();
      if (isFormat(output)) rememberOutput(output.id);
      onPick(output.id);
    },
  },
  h("span", { class: "menu-label" }, output.label),
  h("span", { class: "menu-detail" }, output.detail)));
  // Disabled entries stay readable (they explain why) but are skipped by
  // the arrow keys.
  const items = buttons.filter((_, i) => !options[i].disabled);
  const menu = h("div", { class: choices ? "format-menu plain" : "format-menu", role: "menu", "aria-label": title },
    h("div", { class: "menu-title" }, title), buttons);
  document.body.append(menu);

  if (PHONE.matches) {
    // On phones an anchored popover lands under a thumb or off-screen; dock
    // it to the bottom edge as a sheet instead (positioned by CSS).
    menu.classList.add("sheet");
  } else {
    // Position below the anchor, right-aligned; flip above if it won't fit.
    const rect = anchor.getBoundingClientRect();
    const menuRect = menu.getBoundingClientRect();
    const left = Math.max(8, Math.min(rect.right - menuRect.width, window.innerWidth - menuRect.width - 8));
    const below = rect.bottom + 6;
    const top = below + menuRect.height > window.innerHeight - 8
      ? Math.max(8, rect.top - menuRect.height - 6)
      : below;
    menu.style.left = `${left}px`;
    menu.style.top = `${top}px`;
  }

  const onDocPointer = (event) => {
    if (!menu.contains(event.target) && !anchor.contains(event.target)) closeFormatMenu();
  };
  const onKey = (event) => {
    const index = items.indexOf(document.activeElement);
    if (event.key === "Escape") {
      event.preventDefault();
      closeFormatMenu();
      anchor.focus();
    } else if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      const step = event.key === "ArrowDown" ? 1 : -1;
      items[(index + step + items.length) % items.length].focus();
    }
  };
  document.addEventListener("pointerdown", onDocPointer, true);
  document.addEventListener("keydown", onKey, true);
  window.addEventListener("resize", closeFormatMenu);
  window.addEventListener("scroll", closeFormatMenu, { passive: true });
  anchor.setAttribute("aria-expanded", "true");
  openMenu = { menu, anchor, onDocPointer, onKey };
  const first = items.find((item) => item.classList.contains("selected")) || items[0];
  (first || menu.querySelector(".menu-item")).focus();
}

// ---------- search ----------

function looksLikeUrl(value) {
  return /^https?:\/\//i.test(value.trim());
}

function jobForUrl(url) {
  return state.jobs.find((job) => job.source_url === url && job.state !== "failed" && job.state !== "cancelled");
}

function downloadButton(track) {
  const existing = jobForUrl(track.url);
  if (existing) {
    const done = TERMINAL.has(existing.state);
    const btn = h("button", {
      class: done ? "icon-btn done" : "icon-btn",
      type: "button",
      title: done ? STATE_LABELS[existing.state] : "In download queue",
      "aria-label": done ? STATE_LABELS[existing.state] : "In download queue",
      onclick: () => showView("downloads"),
    }, icon(done ? "check" : "clock"));
    return btn;
  }
  const btn = h("button", {
    class: "icon-btn primary", type: "button",
    title: "Download…", "aria-label": `Download ${track.title}`,
    "aria-haspopup": "menu", "aria-expanded": "false",
  }, icon("download"));
  btn.addEventListener("click", () => openFormatMenu(btn, async (output) => {
    btn.disabled = true;
    const body = {
      url: track.url,
      title: track.title,
      artists: track.artists,
      thumbnail_url: track.thumbnail_url,
    };
    try {
      if (output === UPGRADE_CHOICE) {
        await upgradeFromSearch(body);
      } else {
        await enqueue({ ...body, output });
      }
      btn.replaceWith(h("button", {
        class: "icon-btn", type: "button", title: "In download queue",
        "aria-label": "In download queue", onclick: () => showView("downloads"),
      }, icon("clock")));
    } catch {
      btn.disabled = false;
    }
  }, { extras: [searchUpgradeChoice()] }));
  return btn;
}

/** Soulseek session states in which nothing can be searched. */
const SLSK_ENDED = new Set(["signed_out", "kicked", "rejected"]);

/** The download menu's quality upgrade, with why it can't run if it can't. */
function searchUpgradeChoice() {
  // Unknown until the Soulseek tab has loaded once; the server then explains.
  const signedOut = Boolean(slsk.status) && SLSK_ENDED.has(slsk.status.state);
  return {
    id: UPGRADE_CHOICE,
    label: "Quality upgrade",
    detail: signedOut
      ? "Not possible: sign in to Soulseek in [2] SOULSEEK first"
      : "Skip the stream: find a lossless copy (FLAC, WAV, AIFF) on Soulseek, checked for fakes",
    disabled: signedOut,
  };
}

/** Queue a quality upgrade of a search result, without downloading the stream. */
async function upgradeFromSearch(body) {
  try {
    await api("/api/downloads/upgrade", { method: "POST", body });
    toast(`Looking for a lossless copy of “${body.title}” on Soulseek; progress in Downloads`);
    await pollJobs();
  } catch (err) {
    toast(err.message, true);
    throw err;
  }
}

function artistButton(link, text = link.name) {
  return h("button", {
    class: "artist-link", type: "button", title: `Open ${link.name}'s page`,
    onclick: () => openArtist(link.url),
  }, text);
}

/**
 * Artist names, each clickable when the provider links it to a page. Links
 * that match no credited name (a SoundCloud or YouTube uploader that is a
 * label or re-uploader) are shown after them as "via …".
 */
function artistNames(track) {
  const links = track.artist_links || [];
  const key = (name) => name.toLowerCase();
  const used = new Set();
  const parts = [];
  track.artists.forEach((name, i) => {
    if (i > 0) parts.push(", ");
    const link = links.find((l) => key(l.name) === key(name));
    if (link) used.add(link);
    parts.push(link ? artistButton(link, name) : name);
  });
  if (parts.length === 0) parts.push("Unknown artist");
  for (const link of links.filter((l) => !used.has(l))) parts.push(" · via ", artistButton(link));
  return parts;
}

/** "Artist · 2017 · Label"; a year that is only the upload date says so. */
function trackSubtitle(track) {
  const artists = track.artists.join(", ") || "Unknown artist";
  const year = track.year;
  const uploaded = year && year.basis === "upload";
  const yearText = year ? `${uploaded ? "up. " : ""}${year.year}` : null;
  return h("div", { class: "subtitle", title: [artists, yearText, track.label].filter(Boolean).join(" · ") },
    track.reposted ? h("span", { class: "repost-tag", title: "Reposted by this artist" }, "↻ REPOST · ") : null,
    artistNames(track),
    year ? h("span", {
      class: uploaded ? "year-upload" : null,
      title: uploaded ? `Uploaded in ${year.year}; release date unknown` : `Released in ${year.year}`,
    }, ` · ${yearText}`) : null,
    track.label ? ` · ${track.label}` : null);
}

function trackRow(track) {
  return h("div", { class: "track" },
    playableArt(itemForTrack(track), track.thumbnail_url),
    h("div", { class: "title-cell" },
      h("div", { class: "title", title: track.title }, track.title),
      trackSubtitle(track)),
    h("div", { class: "album" }, track.album || ""),
    h("div", { class: "provider-cell" }, providerTag(track.provider)),
    h("div", { class: "duration" }, formatDuration(track.duration_secs)),
    downloadButton(track));
}

function trackList(tracks) {
  return h("div", { class: "tracks" },
    h("div", { class: "track list-head", "aria-hidden": "true" },
      h("span"), h("span", {}, "NAME"), h("span", { class: "album" }, "ALBUM"),
      h("span", { class: "provider-cell" }, "SRC"), h("span", { class: "duration" }, "TIME"), h("span")),
    tracks.map(trackRow));
}

function listFoot(text) {
  return h("div", { class: "list-foot" }, text);
}

/** Says how many listed tracks the server left out for their length. */
function tooLongFoot(tooLong) {
  if (!tooLong) return null;
  return h("div", { class: "filter-foot" },
    `${tooLong.count} OVER ${formatDuration(tooLong.max_secs)} HIDDEN (MIXES / SETS)`);
}

function skeleton(n = 6) {
  return Array.from({ length: n }, () => h("div", { class: "skeleton" }));
}

// ----- search filters -----

/** "4:30", "4" (minutes) or "" → seconds, "" or null when unparseable. */
function parseLength(raw) {
  const text = raw.trim();
  if (!text) return "";
  const m = /^(\d{1,4})(?::([0-5]?\d))?$/.exec(text);
  return m ? String(Number(m[1]) * 60 + Number(m[2] || 0)) : null;
}

/**
 * The filter inputs as API parameters, or throws with a readable reason and
 * the offending input as `field`.
 */
function readFilters() {
  const params = {};
  const set = (key, value) => { if (value) params[key] = value; };
  set("year_from", $("#f-year-from").value.trim());
  set("year_to", $("#f-year-to").value.trim());
  set("label", $("#f-label").value.trim());
  set("exclude", $("#f-exclude").value.trim());
  for (const [id, key, name] of [["#f-min", "min_duration", "minimum"], ["#f-max", "max_duration", "maximum"]]) {
    const secs = parseLength($(id).value);
    if (secs === null) {
      throw Object.assign(new Error(`The ${name} length must look like 4:30 or 4 (minutes).`), { field: $(id) });
    }
    set(key, secs);
  }
  const count = Object.keys(params).length;
  if (count && $("#f-unknown").checked) params.include_unknown = "true";
  return { params, count };
}

function syncFiltersToggle() {
  const open = !$("#search-filters").hidden;
  let count = 0;
  try { count = readFilters().count; } catch { count = 1; }
  const toggle = $("#filters-toggle");
  toggle.textContent = `[${open ? "-" : "+"}] FILTERS${count ? ` (${count})` : ""}`;
  toggle.setAttribute("aria-expanded", String(open));
  toggle.classList.toggle("on", count > 0);
}

function setupSearchFilters() {
  const panel = $("#search-filters");
  $("#filters-toggle").addEventListener("click", () => {
    panel.hidden = !panel.hidden;
    syncFiltersToggle();
    if (!panel.hidden) $("#f-year-from").focus();
  });
  panel.addEventListener("input", syncFiltersToggle);
  panel.addEventListener("submit", (event) => {
    event.preventDefault();
    $("#search-form").requestSubmit();
  });
  $("#filters-clear").addEventListener("click", () => {
    panel.reset();
    syncFiltersToggle();
    if (state.lastSearch) $("#search-form").requestSubmit();
  });
}

async function runSearch(event) {
  event.preventDefault();
  const value = $("#search-input").value.trim();
  let filters;
  try {
    filters = readFilters();
  } catch (err) {
    // The panel may be closed, and the value may not even be the user's
    // (password managers fill fields despite autocomplete="off"): show it.
    if (err.field) {
      $("#search-filters").hidden = false;
      err.field.focus();
      err.field.select();
    }
    syncFiltersToggle();
    toast(err.message, true);
    return;
  }
  // A label alone is a search too: the server searches for the label.
  if (!value && !filters.params.label) return;
  const results = $("#search-results");
  results.replaceChildren(...skeleton());
  const seq = ++state.resultsSeq;
  state.backToResults = null;
  try {
    if (looksLikeUrl(value)) {
      state.lastSearch = null;
      const resolved = await api("/api/resolve", { method: "POST", body: { url: value } });
      if (seq !== state.resultsSeq) return;
      renderResolved(resolved);
      state.backToResults = () => renderResolved(resolved);
      if (filters.count) {
        results.prepend(h("p", { class: "notice" }, "FILTERS APPLY TO SEARCHES, NOT TO LINKS."));
      }
    } else {
      const params = new URLSearchParams({ q: value, provider: state.provider, ...filters.params });
      state.lastSearch = params.toString();
      const response = await api(`/api/search?${params}`);
      if (seq !== state.resultsSeq) return;
      renderSearch(response);
      state.backToResults = () => renderSearch(response);
    }
  } catch (err) {
    if (seq !== state.resultsSeq) return;
    results.replaceChildren(h("div", { class: "empty" },
      h("p", { class: "empty-title" }, ">> NOTHING TO SHOW."), h("p", { class: "error-line" }, err.message)));
  }
}

function renderSearch(response) {
  const results = $("#search-results");
  const children = response.errors.map((e) =>
    h("p", { class: "notice" }, `${PROVIDER_NAMES[e.provider] || e.provider}: ${e.message}`));
  const foot = filterFoot(response.filtered);
  const longFoot = tooLongFoot(response.too_long);
  if (response.results.length === 0) {
    children.push(h("div", { class: "empty" }, h("p", { class: "empty-title" }, ">> 0 FILES FOUND."),
      h("p", {}, response.filtered
        ? "NOTHING PASSED THE FILTERS. LOOSEN THEM OR TRY DIFFERENT WORDS."
        : "TRY DIFFERENT WORDS, OR PASTE A LINK DIRECTLY.")));
    if (foot) children.push(foot);
    if (longFoot) children.push(longFoot);
    results.replaceChildren(...children);
    return;
  }
  const groups = new Map();
  for (const track of response.results) {
    if (!groups.has(track.provider)) groups.set(track.provider, []);
    groups.get(track.provider).push(track);
  }
  for (const [provider, tracks] of groups) {
    children.push(
      h("div", { class: "section-title" }, provider === "youtube" ? "YOUTUBE-MUSIC" : PROVIDER_NAMES[provider] || provider),
      trackList(tracks));
  }
  children.push(listFoot(`${response.results.length} FILES   ${groups.size} DIRS`));
  if (foot) children.push(foot);
  if (longFoot) children.push(longFoot);
  results.replaceChildren(...children);
}

/** What the filters left out, with a way to see the undecidable ones. */
function filterFoot(filtered) {
  if (!filtered || (!filtered.hidden && !filtered.unknown)) return null;
  const parts = [];
  if (filtered.hidden) parts.push(`${filtered.hidden} FILTERED OUT`);
  if (filtered.unknown) parts.push(`${filtered.unknown} WITH UNKNOWN YEAR / LABEL / LENGTH`);
  const showUnknown = filtered.unknown
    ? h("button", { class: "btn", type: "button", onclick: () => {
      $("#f-unknown").checked = true;
      syncFiltersToggle();
      $("#search-form").requestSubmit();
    } }, "[ SHOW THEM ]")
    : null;
  return h("div", { class: "filter-foot" }, `${parts.join(" · ")} (OF ${filtered.scanned} LOOKED AT) `, showUnknown);
}

function renderResolved(resolved) {
  const results = $("#search-results");
  if (resolved.kind === "track") {
    results.replaceChildren(h("div", { class: "section-title" }, "TRACK"), trackList([resolved]));
    return;
  }
  const first = resolved.tracks.find((t) => t.thumbnail_url);
  const downloadAll = h("button", { class: "btn btn-primary", type: "button" },
    `[ GET ALL ${resolved.tracks.length} ]`);
  downloadAll.disabled = resolved.tracks.length === 0;
  downloadAll.setAttribute("aria-haspopup", "menu");
  downloadAll.addEventListener("click", () => openFormatMenu(downloadAll, async (output) => {
    downloadAll.disabled = true;
    try {
      await enqueue({ url: resolved.url, output });
      renderResolved(resolved);
    } catch {
      downloadAll.disabled = false;
    }
  }));
  results.replaceChildren(
    h("div", { class: "collection-head" },
      art(first && first.thumbnail_url),
      h("div", {},
        h("div", { class: "eyebrow" }, `RELEASE PACK <${PROVIDER_NAMES[resolved.provider] || resolved.provider}>`),
        h("div", { class: "coll-title" }, resolved.title || "Untitled collection"),
        h("div", { class: "dim" }, `${resolved.tracks.length} FILES`),
        h("div", {}, downloadAll))),
    trackList(resolved.tracks),
    listFoot(`${resolved.tracks.length} FILES`),
    ...[tooLongFoot(resolved.too_long)].filter(Boolean));
}

// ----- artist pages -----

function includeReposts() {
  try {
    return localStorage.getItem(REPOSTS_KEY) !== "off";
  } catch {
    return true;
  }
}

function rememberReposts(on) {
  try { localStorage.setItem(REPOSTS_KEY, on ? "on" : "off"); } catch { /* storage unavailable */ }
}

/** Show an artist's (or uploader's) page in place of the search results. */
async function openArtist(url) {
  const results = $("#search-results");
  const seq = ++state.resultsSeq;
  results.replaceChildren(...[backButton(), ...skeleton()].filter(Boolean));
  results.scrollIntoView({ block: "start" });
  try {
    const reposts = String(includeReposts());
    const page = await api(`/api/artist?${new URLSearchParams({ url, reposts })}`);
    if (seq !== state.resultsSeq) return;
    renderArtist(page);
  } catch (err) {
    if (seq !== state.resultsSeq) return;
    results.replaceChildren(...[backButton(), h("div", { class: "empty" },
      h("p", { class: "empty-title" }, ">> ARTIST PAGE UNAVAILABLE."),
      h("p", { class: "error-line" }, err.message))].filter(Boolean));
  }
}

function backButton() {
  const back = state.backToResults;
  if (!back) return null;
  return h("div", { class: "artist-nav" }, h("button", {
    class: "btn", type: "button",
    onclick: () => { state.resultsSeq += 1; back(); },
  }, "[ < BACK TO RESULTS ]"));
}

/** Long biographies start folded so the track list stays in view. */
function biography(text) {
  const bio = h("p", { class: "bio" }, text);
  if (text.length <= 400 && text.split("\n").length <= 6) return bio;
  bio.classList.add("folded");
  const toggle = h("button", { class: "btn", type: "button", "aria-expanded": "false" }, "[ MORE ]");
  toggle.addEventListener("click", () => {
    const folded = bio.classList.toggle("folded");
    toggle.textContent = folded ? "[ MORE ]" : "[ LESS ]";
    toggle.setAttribute("aria-expanded", String(!folded));
  });
  return h("div", {}, bio, toggle);
}

/** Switch for listing reposts, on providers that have them (SoundCloud). */
function repostsToggle(page) {
  if (typeof page.reposts !== "boolean") return null;
  const on = page.reposts;
  return h("div", { class: "artist-nav" }, h("button", {
    class: on ? "btn reposts-toggle on" : "btn reposts-toggle", type: "button",
    "aria-pressed": String(on),
    title: on ? "Showing tracks this artist reposted · click to hide them"
      : "Hiding tracks this artist reposted · click to show them",
    onclick: () => { rememberReposts(!on); openArtist(page.url); },
  }, `[${on ? "x" : " "}] REPOSTS`));
}

function renderArtist(page) {
  const site = page.provider === "youtube" && page.url.startsWith("https://music.youtube.com/")
    ? "YOUTUBE MUSIC" : (PROVIDER_NAMES[page.provider] || page.provider).toUpperCase();
  const reposted = page.tracks.filter((t) => t.reposted).length;
  const count = `${page.truncated ? "FIRST " : ""}${page.tracks.length} FILES${reposted ? ` · ${reposted} REPOSTED` : ""}`;
  const original = page.url.startsWith("https://")
    ? h("a", { class: "btn", href: page.url, target: "_blank", rel: "noopener noreferrer" },
      `[ OPEN ON ${site} ↗ ]`)
    : null;
  const children = [
    backButton(),
    h("div", { class: "collection-head" },
      art(page.image_url),
      h("div", {},
        h("div", { class: "eyebrow" }, `ARTIST <${PROVIDER_NAMES[page.provider] || page.provider}>`),
        h("div", { class: "coll-title" }, page.name),
        h("div", { class: "dim" }, count),
        h("div", {}, original))),
    h("div", { class: "section-title" }, "BIO"),
    page.biography ? biography(page.biography) : h("p", { class: "dim" }, "NO BIOGRAPHY ON THIS PAGE."),
    h("div", { class: "section-title" }, page.reposts ? "TRACKS + REPOSTS" : "TRACKS"),
    repostsToggle(page),
  ];
  if (page.tracks.length) {
    children.push(trackList(page.tracks), listFoot(page.truncated
      ? `${page.tracks.length} FILES SHOWN · MORE ON ${site}`
      : `${page.tracks.length} FILES`));
  } else {
    children.push(h("p", { class: "dim" }, page.reposts ? "NO TRACKS POSTED OR REPOSTED." : "NO TRACKS POSTED."));
  }
  children.push(tooLongFoot(page.too_long));
  $("#search-results").replaceChildren(...children.filter(Boolean));
}

async function enqueue(body) {
  try {
    const res = await api("/api/downloads", { method: "POST", body });
    toast(res.warning || (res.job_ids.length > 1
      ? `Added ${res.job_ids.length} tracks to downloads`
      : "Added to downloads"), Boolean(res.warning));
    await pollJobs();
    return res;
  } catch (err) {
    toast(err.message, true);
    throw err;
  }
}

// ---------- downloads ----------

// The ASCII bar is fixed-width text, so it has to shrink on narrow screens
// rather than be clipped (which would hide the percentage).
const NARROW = window.matchMedia("(max-width: 480px)");
NARROW.addEventListener("change", () => { if (state.view === "downloads") renderJobs(); });

function progressBar(job) {
  const active = !TERMINAL.has(job.state) && job.state !== "queued";
  if (!active) return null;
  const BAR_WIDTH = NARROW.matches ? 16 : 30;
  if (job.state === "downloading" && typeof job.progress_fraction === "number") {
    const fraction = Math.min(1, Math.max(0, job.progress_fraction));
    const filled = Math.round(fraction * BAR_WIDTH);
    const percent = Math.round(fraction * 100);
    return h("div", { class: "progress", role: "progressbar",
      "aria-valuemin": "0", "aria-valuemax": "100", "aria-valuenow": String(percent) },
    `[${"#".repeat(filled)}${".".repeat(BAR_WIDTH - filled)}] ${String(percent).padStart(3)}%`);
  }
  return h("div", { class: "progress indeterminate" }, `[${"=".repeat(BAR_WIDTH)}] ${STATE_LABELS[job.state] || job.state}`);
}

function fact(label, value) {
  return value ? h("span", {}, `${label} `, h("b", {}, value)) : null;
}

function jobFacts(job) {
  const meta = job.metadata || {};
  const facts = [];
  const p = job.progress;
  if (job.state === "downloading" && p) {
    const size = `${formatBytes(p.downloaded_bytes)} / ${formatBytes(p.total_bytes)}`;
    const speed = p.speed_bytes_per_sec ? ` · ${formatBytes(p.speed_bytes_per_sec)}/s` : "";
    facts.push(h("span", {}, size + speed));
  }
  facts.push(fact("Source", job.source_format_summary));
  if (job.quality) {
    facts.push(h("span", {}, "File ", h("b", {}, job.quality.summary),
      job.quality.lossless ? h("span", { class: "lossless" }, " · lossless") : null,
      job.quality.converted_from
        ? ` · converted from ${job.quality.converted_from}`
        : " · not re-encoded"));
    if (job.quality.spectrum) facts.push(fact("Spectrum", job.quality.spectrum.summary));
  }
  facts.push(fact("Album", meta.album));
  if (meta.release_date) {
    const d = meta.release_date;
    facts.push(fact("Released", [d.year, d.month, d.day].filter(Boolean)
      .map((v, i) => (i ? String(v).padStart(2, "0") : v)).join("-")));
  }
  facts.push(fact("Label", meta.label));
  if (job.requested_by && job.requested_by.length) facts.push(fact("Crew", job.requested_by.join(", ")));
  if (meta.genres && meta.genres.length) facts.push(fact("Genre", meta.genres.join(", ")));
  if (job.artwork_embedded != null) {
    facts.push(fact("Artwork", job.artwork_embedded ? "embedded" : "none"));
  }
  return facts.filter(Boolean);
}

/** URL serving a job's library file, or null if there is no file. */
function fileUrl(job) {
  if (!job.destination || !job.track_key) return null;
  return libraryFileUrl(job.track_key.provider, job.track_key.id, job.output);
}

/** URL serving a library file by its identity and format. */
function libraryFileUrl(provider, id, output) {
  return `/api/files/${encodeURIComponent(provider)}/${encodeURIComponent(id)}/${encodeURIComponent(output)}`;
}

/** Link that sends a library file to this device under its library name. */
function saveLink(url, path, title) {
  const name = path.split(/[\\/]/).pop();
  return h("a", {
    class: "icon-btn save", href: url, download: name,
    title: `Save “${name}” to this device`, "aria-label": `Save ${title} to this device`,
  }, icon("save"));
}

function saveButton(job) {
  const url = fileUrl(job);
  return url ? saveLink(url, job.destination, job.title) : h("span");
}

function stateLabel(job) {
  if (job.kind === "convert" && job.state === "processing") return "Converting";
  if (job.kind === "upgrade" && job.state === "resolving") return "Searching";
  if (job.kind === "upgrade" && job.state === "processing") return "Verifying";
  return STATE_LABELS[job.state] || job.state;
}

/** Subtitle of jobs that do not download from the track's own provider. */
const JOB_KIND_LABELS = {
  convert: "conversion of a library file",
  upgrade: "quality upgrade from Soulseek",
};

function jobCard(job) {
  const meta = job.metadata;
  const inferred = meta && meta.inferred && meta.inferred.length
    ? h("div", { class: "warn-line" },
      `Guessed from the upload title — please check: ${meta.inferred.map((f) => f.replace("_", " ")).join(", ")}`)
    : null;
  const cancel = TERMINAL.has(job.state) ? saveButton(job) : h("button", {
    class: "icon-btn", type: "button", title: "Cancel", "aria-label": `Cancel ${job.title}`,
    onclick: async () => {
      try { await api(`/api/downloads/${job.id}/cancel?scope=${jobsScope()}`, { method: "POST" }); await pollJobs(); }
      catch (err) { toast(err.message, true); }
    },
  }, icon("close"));

  return h("article", { class: "job", "data-id": String(job.id) },
    playableArt(itemForJob(job), job.thumbnail_url, "art lg"),
    h("div", { class: "job-main" },
      h("div", { class: "job-top" },
        h("div", { class: "title", title: job.title }, job.title),
        h("span", { class: "format-pill", title: "Requested format" }, OUTPUT_SHORT[job.output] || job.output),
        h("span", { class: `state ${job.state}` }, stateLabel(job))),
      h("div", { class: "subtitle" },
        `${job.artists.join(", ") || "Unknown artist"} · ${JOB_KIND_LABELS[job.kind]
          || PROVIDER_NAMES[job.provider] || job.provider}`),
      progressBar(job),
      h("div", { class: "facts" }, jobFacts(job)),
      job.destination ? h("div", { class: "path", title: "Location in library" }, job.destination) : null,
      inferred,
      job.note ? h("div", { class: "note" }, job.note) : null,
      job.warnings.map((w) => h("div", { class: "warn-line" }, w)),
      job.error ? h("div", { class: "error-line" },
        `Failed while ${STATE_LABELS[job.error.stage]?.toLowerCase() || job.error.stage}: ${job.error.message}` +
        (job.error.retryable ? " (temporary — try again later)" : "")) : null),
    cancel);
}

function renderJobs() {
  const container = $("#jobs");
  const active = state.jobs.filter((job) => !TERMINAL.has(job.state)).length;
  const badge = $("#active-badge");
  badge.hidden = active === 0;
  badge.textContent = String(active);
  if (state.view !== "downloads") return;

  if (state.jobs.length === 0) {
    state.jobEls.clear();
    container.replaceChildren(h("div", { class: "empty" }, h("p", { class: "empty-title" }, ">> QUEUE EMPTY."),
      h("p", {}, "SEARCH FOR A TRACK AND HIT [GET].")));
    return;
  }
  // Rebuild only cards whose job changed so images and focus stay stable.
  const next = new Map();
  const els = state.jobs.map((job) => {
    const key = String(job.id);
    const previous = state.jobEls.get(key);
    const el = previous && previous.dataset.updated === String(job.updated_at_ms)
      ? previous
      : jobCard(job);
    el.dataset.updated = String(job.updated_at_ms);
    next.set(key, el);
    return el;
  });
  state.jobEls = next;
  container.replaceChildren(...els);
}

/** Refresh the library view when a job finishes adding a track to it. */
function refreshOnTransitions(previous, jobs) {
  // The first snapshot is history, not news.
  if (!previous) return;
  for (const job of jobs) {
    const before = previous.get(job.id);
    if (before === job.state || !TERMINAL.has(job.state)) continue;
    if ((job.state === "completed" || job.state === "skipped") && state.view === "library") loadLibrary();
  }
}

/** Captains may look at every crew member's transfers; everyone else sees their own. */
function jobsScope() {
  return isCaptain() ? state.jobsScope : "mine";
}

async function pollJobs() {
  try {
    const scope = jobsScope();
    const jobs = await api(`/api/downloads?scope=${scope}`);
    // The SHOW chips changed while this was in flight: a newer poll owns the list.
    if (scope !== jobsScope()) return;
    refreshOnTransitions(state.jobStates, jobs);
    state.jobStates = new Map(jobs.map((job) => [job.id, job.state]));
    state.jobs = jobs;
    renderJobs();
  } catch {
    // Transient; the next poll retries.
  }
}

function schedulePoll(delay) {
  clearTimeout(state.pollTimer);
  if (!state.user) return;
  state.pollTimer = setTimeout(async () => {
    await pollJobs();
    const active = state.jobs.some((job) => !TERMINAL.has(job.state));
    schedulePoll(active ? 1000 : state.view === "downloads" ? 3000 : 6000);
  }, delay);
}

// ---------- library ----------

async function loadLibrary() {
  const container = $("#library");
  if (state.library.length === 0) container.replaceChildren(...skeleton(4));
  try {
    const scope = isCaptain() ? state.libraryScope : "mine";
    state.library = await api(`/api/library?scope=${scope}`);
    const present = new Set(state.library.map((entry) => trackIdentity(entry.key)));
    for (const id of state.librarySelected) if (!present.has(id)) state.librarySelected.delete(id);
    renderLibrary();
  } catch (err) {
    container.replaceChildren(h("p", { class: "error-line" }, err.message));
  }
}

function renderLibrary() {
  const container = $("#library");
  const filter = $("#library-filter").value.trim().toLowerCase();
  if (state.library.length === 0) {
    state.libraryShown = [];
    container.replaceChildren(h("div", { class: "empty" }, h("p", { class: "empty-title" }, ">> 0 FILES."),
      h("p", {}, state.libraryScope === "all" && isCaptain()
        ? "THE HOLD IS EMPTY."
        : "TRACKS YOU DOWNLOAD APPEAR HERE. TRACKS ALREADY IN THE HOLD ARE ADDED WITHOUT DOWNLOADING AGAIN.")));
    return;
  }
  const tracks = groupLibrary(state.library);
  const shown = sortLibrary(tracks).filter((versions) => !filter || versions.some((entry) =>
    [entry.title, entry.album, entry.path, ...entry.artists, ...(entry.owned_by || [])]
      .some((v) => v && v.toLowerCase().includes(filter))));
  const files = (list) => list.reduce((n, versions) => n + versions.length, 0);
  const count = (list) => `${list.length} TRACK${list.length === 1 ? "" : "S"}` +
    (files(list) === list.length ? "" : ` · ${files(list)} FILES`);
  state.libraryShown = shown.map((versions) => trackIdentity(versions[0].key));
  // One grid for the heading and every row (rows are subgrids), so the
  // columns line up whatever each row's buttons and text need.
  container.replaceChildren(
    h("div", { class: "lib-list" },
      h("div", { class: "lib-row list-head", "aria-hidden": "true" }, h("span"), h("span"), h("span", {}, "NAME"),
        h("span", { class: "album" }, "ALBUM"), h("span", { class: "added-cell" }, "ADDED"),
        h("span", { class: "quality-cell" }, "QUALITY"), h("span")),
      ...shown.map(libraryRow)),
    h("div", { class: "lib-bulk", id: "lib-bulk", role: "toolbar", "aria-label": "Bulk actions" }),
    listFoot(filter ? `${count(shown)} OF ${count(tracks)} MATCH` : count(tracks)));
  renderBulkBar();
}

function trackIdentity(key) {
  return `${key.provider}:${key.id}`;
}

/**
 * The library as one list per track: every file of the same provider
 * identity (the original, its conversions and a lossless upgrade) together,
 * the upgrade first (it is the best copy), then in format-list order.
 * Tracks keep the order of their newest file.
 */
function groupLibrary(entries) {
  const groups = new Map();
  for (const entry of entries) {
    const id = trackIdentity(entry.key);
    if (!groups.has(id)) groups.set(id, []);
    groups.get(id).push(entry);
  }
  const rank = (entry) => {
    if (entry.output === "lossless") return -1;
    const i = OUTPUTS.findIndex((o) => o.id === entry.output);
    return i < 0 ? OUTPUTS.length : i;
  };
  return [...groups.values()].map((versions) => versions.sort((a, b) => rank(a) - rank(b)));
}

/**
 * Library orders. Each has the direction people mostly want (newest and
 * best first, names A–Z); picking the active one again reverses it.
 */
const LIBRARY_SORTS = {
  added: (a, b) => trackAdded(b) - trackAdded(a),
  title: (a, b) => compareText(a[0].title, b[0].title),
  artist: (a, b) => compareText(a[0].artists.join(", "), b[0].artists.join(", "))
    || compareText(a[0].album, b[0].album) || compareText(a[0].title, b[0].title),
  album: (a, b) => compareText(a[0].album, b[0].album) || compareText(a[0].title, b[0].title),
  quality: (a, b) => trackQuality(b) - trackQuality(a),
};

/** What each order puts first, as [usual, reversed]. */
const SORT_DIRECTIONS = {
  added: ["NEWEST", "OLDEST"],
  title: ["A–Z", "Z–A"],
  artist: ["A–Z", "Z–A"],
  album: ["A–Z", "Z–A"],
  quality: ["BEST", "WORST"],
};

const collator = new Intl.Collator(undefined, { sensitivity: "base", numeric: true });

/** Names in collation order; a missing one (a single's album) sorts last. */
function compareText(a, b) {
  if (!a || !b) return Number(!a) - Number(!b);
  return collator.compare(a, b);
}

/** When the track first entered the library: its first file, before any conversion. */
function trackAdded(versions) {
  return Math.min(...versions.map((v) => v.added_at));
}

/** The track's best file, by the real quality the server ranks. */
function trackQuality(versions) {
  return Math.max(...versions.map((v) => v.quality_rank));
}

function storedLibrarySort() {
  let stored = "";
  try { stored = localStorage.getItem(LIBRARY_SORT_KEY) || ""; } catch { /* storage unavailable */ }
  const [by, reversed] = stored.split(":");
  return LIBRARY_SORTS[by] ? { by, reversed: reversed === "reversed" } : { by: "added", reversed: false };
}

function rememberLibrarySort() {
  const { by, reversed } = state.librarySort;
  try { localStorage.setItem(LIBRARY_SORT_KEY, reversed ? `${by}:reversed` : by); } catch { /* storage unavailable */ }
}

/** Tracks in the chosen order; ties stay newest first. */
function sortLibrary(tracks) {
  const { by, reversed } = state.librarySort;
  const compare = LIBRARY_SORTS[by];
  const sign = reversed ? -1 : 1;
  return [...tracks].sort((a, b) => sign * compare(a, b) || LIBRARY_SORTS.added(a, b));
}

/** Marks the active sort chip, with an arrow saying which way it runs. */
function renderLibrarySort() {
  const { by, reversed } = state.librarySort;
  for (const chip of document.querySelectorAll("#library-sort .chip")) {
    const active = chip.dataset.sort === by;
    chip.classList.toggle("active", active);
    chip.setAttribute("aria-checked", String(active));
    chip.querySelector(".sort-dir").textContent = active ? ` · ${SORT_DIRECTIONS[by][Number(reversed)]}` : "";
    chip.title = active ? "Click again to reverse" : "";
  }
}

function addedCell(versions) {
  const added = trackAdded(versions);
  if (!added) return h("div", { class: "added-cell" }, "--");
  return h("div", { class: "added-cell", title: `Added ${formatClock(added * 1000)}` },
    h("span", { class: "added-label" }, "added "), formatAdded(added));
}

/** A day: TODAY and YESTERDAY read faster than this week's dates. */
function formatAdded(secs) {
  const day = (d) => new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
  const days = Math.round((day(new Date()) - day(new Date(secs * 1000))) / 86_400_000);
  if (days === 0) return "TODAY";
  if (days === 1) return "YESTERDAY";
  return formatDate(secs);
}

/** The file a library track's row acts on: the format picked in its menu. */
function pickedVersion(versions) {
  const id = trackIdentity(versions[0].key);
  return versions.find((v) => v.output === state.libraryFormat.get(id)) || versions[0];
}

/** One library track; with several formats, a menu picks the file the row acts on. */
function libraryRow(versions) {
  const id = trackIdentity(versions[0].key);
  const entry = pickedVersion(versions);
  const formatLabel = (output) => OUTPUT_SHORT[output] || output;
  const picker = versions.length > 1
    ? h("select", {
      class: "format-select",
      title: `${versions.length} formats of this track`,
      "aria-label": `Format of ${entry.title}`,
      onchange: (event) => {
        state.libraryFormat.set(id, event.target.value);
        const next = libraryRow(versions);
        row.replaceWith(next);
        const select = next.querySelector(".format-select");
        if (select) select.focus();
      },
    }, versions.map((v) => h("option", { value: v.output, selected: v === entry ? true : null },
      formatLabel(v.output))))
    : null;
  const selected = state.librarySelected.has(id);
  const row = h("div", { class: selected ? "lib-row selected" : "lib-row", "data-track": id },
    h("button", {
      type: "button", class: "pick", role: "checkbox", "aria-checked": String(selected),
      "aria-label": `Select ${entry.title}`, title: "Select for bulk actions (shift-click: a range)",
      onclick: (event) => toggleSelected(id, event.shiftKey),
    }),
    playableArt(itemForEntry(entry), entry.artwork_url),
    h("div", { class: "title-cell" },
      h("div", { class: "title", title: entry.title }, entry.title),
      h("div", { class: "subtitle" }, entry.artists.join(", ")),
      entry.owned_by ? h("div", { class: "owners" }, entry.owned_by.length
        ? `crew: ${entry.owned_by.join(", ")}`
        : "crew: nobody (from before accounts)") : null),
    h("div", { class: "album" }, entry.album || "--"),
    addedCell(versions),
    h("div", { class: "quality-cell", title: qualityTitle(entry) },
      picker,
      // A wrapped quality line breaks after a "·", never before it.
      h("span", { class: "q-summary" }, keepUnits(entry.quality.summary).replace(/ · /g, "\u00a0· ")),
      entry.quality.converted_from ? h("div", { class: "converted" }, `from ${entry.quality.converted_from}`) : null,
      entry.obtained_from && entry.obtained_from.startsWith("slsk://")
        ? h("div", { class: "converted" }, "via Soulseek") : null,
      spectrumLine(entry.quality),
      entry.quality.lossless ? h("div", { class: "lossless" }, "lossless") : null),
    libraryFacts(entry, versions),
    rowActions(entry, versions));
  return row;
}

/**
 * The row's small print on one line, for phones, where the quality,
 * added and crew columns would each take a line: the quality cell keeps
 * only the format picker and summary there.
 */
function libraryFacts(entry, versions) {
  const fact = (text, cls = "") => h("span", { class: `fact ${cls}`.trim() }, text);
  const check = entry.quality.spectrum;
  const added = trackAdded(versions);
  const facts = [
    entry.quality.lossless ? fact("lossless", "lossless") : null,
    check ? fact(SPECTRUM_SHORT[check.verdict] || check.verdict,
      check.verdict === "lossy" || check.verdict === "upsampled" ? "bad" : "") : null,
    entry.quality.converted_from ? fact(`from ${entry.quality.converted_from}`) : null,
    entry.obtained_from && entry.obtained_from.startsWith("slsk://") ? fact("via Soulseek") : null,
    added ? fact(formatAdded(added)) : null,
    entry.owned_by ? fact(entry.owned_by.length ? `crew: ${entry.owned_by.join(", ")}` : "crew: nobody") : null,
  ].filter(Boolean);
  // Plain spaces between the facts are where the line may wrap.
  return h("div", { class: "lib-facts", "aria-hidden": "true" },
    ...facts.flatMap((f, i) => (i ? [" ", f] : [f])));
}

/**
 * The row's buttons. On phones they fold into one [...] button opening a
 * sheet, so each row is not a toolbar; picking an action presses the
 * (hidden) button, which then works as on wide screens.
 */
function rowActions(entry, versions) {
  const share = shareButton(entry);
  const convert = convertButton(entry);
  const save = saveLink(libraryFileUrl(entry.key.provider, entry.key.id, entry.output), String(entry.path), entry.title);
  const remove = removeButton(entry, versions.length);
  const more = h("button", {
    class: "icon-btn more-btn", type: "button", title: "Actions…",
    "aria-label": `Actions for ${entry.title}`, "aria-haspopup": "menu", "aria-expanded": "false",
  }, "…");
  const format = OUTPUT_SHORT[entry.output] || entry.output;
  more.addEventListener("click", () => {
    const actions = { share, convert, save, remove };
    openFormatMenu(more, (id) => actions[id].click(), {
      title: entry.title,
      choices: [
        { id: "share", label: "Send to a chat room", detail: "Share it with a room you are in" },
        { id: "convert", label: convert.title.replace(/…$/, ""), detail: `From ${format}`, disabled: convert.disabled },
        { id: "save", label: "Save to this device", detail: String(entry.path).split(/[\\/]/).pop() },
        { id: "remove", label: "Delete", detail: `This format (${format}); asks first` },
      ],
    });
  });
  return h("div", { class: "row-actions" }, share, convert, save, remove, more);
}

// ---------- library bulk actions ----------
// Ticked tracks are acted on together, each in the format its row shows.
// Every action goes through the same server checks as the row buttons.

/** Tick or untick a track; with shift, every listed track up to the last one ticked. */
function toggleSelected(id, range) {
  const selected = state.librarySelected;
  const on = !selected.has(id);
  const from = state.libraryShown.indexOf(state.libraryAnchor);
  const to = state.libraryShown.indexOf(id);
  const ids = range && from >= 0 && to >= 0
    ? state.libraryShown.slice(Math.min(from, to), Math.max(from, to) + 1)
    : [id];
  for (const each of ids) {
    if (on) selected.add(each); else selected.delete(each);
  }
  state.libraryAnchor = id;
  syncSelectedRows();
}

function setSelection(ids) {
  state.librarySelected = new Set(ids);
  state.libraryAnchor = null;
  syncSelectedRows();
}

/** Update the rows' ticks and the bulk bar without rebuilding the list. */
function syncSelectedRows() {
  for (const row of document.querySelectorAll("#library .lib-row[data-track]")) {
    const on = state.librarySelected.has(row.dataset.track);
    row.classList.toggle("selected", on);
    row.querySelector(".pick").setAttribute("aria-checked", String(on));
  }
  renderBulkBar();
}

/** The ticked tracks, each as its picked file. */
function selectedEntries() {
  return groupLibrary(state.library)
    .filter((versions) => state.librarySelected.has(trackIdentity(versions[0].key)))
    .map(pickedVersion);
}

function fileRef(entry) {
  return { provider: entry.key.provider, id: entry.key.id, output: entry.output };
}

function plural(n, word) {
  return `${n} ${word}${n === 1 ? "" : "S"}`;
}

function renderBulkBar() {
  const bar = $("#lib-bulk");
  if (!bar) return;
  const shown = state.libraryShown;
  const picked = state.librarySelected.size;
  const shownPicked = shown.filter((id) => state.librarySelected.has(id)).length;
  const all = shown.length > 0 && shownPicked === shown.length;
  const hidden = picked - shownPicked;
  const busy = state.libraryBusy;
  // Phones fit the four actions on one line only without the inner spaces.
  const action = (label, title, run, cls = "btn") => h("button", {
    type: "button", class: cls, title, disabled: !picked || busy ? true : null,
    onclick: (event) => run(event.currentTarget),
  }, PHONE.matches ? `[${label}]` : `[ ${label} ]`);
  bar.classList.toggle("active", picked > 0);
  bar.replaceChildren(
    h("button", {
      type: "button", class: "pick", role: "checkbox",
      "aria-checked": all ? "true" : shownPicked ? "mixed" : "false",
      "aria-label": "Select every listed track",
      disabled: shown.length && !busy ? null : true,
      onclick: () => {
        const others = [...state.librarySelected].filter((id) => !shown.includes(id));
        setSelection(all ? others : [...others, ...shown]);
      },
    }),
    h("span", { class: picked ? "bulk-count" : "bulk-count dim" },
      picked
        ? `${plural(picked, "TRACK")} SELECTED` + (hidden ? ` (${hidden} HIDDEN BY FILTER)` : "")
        : PHONE.matches ? "SELECT ALL" : "SELECT TRACKS FOR BULK ACTIONS"),
    h("span", { class: "bulk-clear" }, action("CLEAR", "Untick every track", () => setSelection([]))),
    h("span", { class: "bulk-actions" },
      action("SAVE", "Save the selected files to this device (one ZIP)", bulkSave),
      action("SEND", "Send the selected tracks to a chat room…", bulkSend),
      action("CONV", "Convert or upgrade the selected tracks…", bulkConvert),
      action("DEL", state.libraryScope === "all" && isCaptain()
        ? "Delete the selected files from the hold for everyone…"
        : "Remove the selected files from your library…", bulkRemove, "btn danger")));
}

/** Run a bulk action with the bar's buttons disabled meanwhile. */
async function bulkBusy(run) {
  if (state.libraryBusy) return;
  state.libraryBusy = true;
  renderBulkBar();
  try {
    await run();
  } catch (err) {
    toast(err.message, true);
  } finally {
    state.libraryBusy = false;
    renderBulkBar();
  }
}

/** Save the ticked files: one file as itself, several as one ZIP the browser streams to disk. */
function bulkSave() {
  const entries = selectedEntries();
  if (entries.length === 1) {
    const [entry] = entries;
    saveLink(libraryFileUrl(entry.key.provider, entry.key.id, entry.output), String(entry.path), entry.title).click();
    return;
  }
  bulkBusy(async () => {
    const ticket = await api("/api/library/archive", { method: "POST", body: { files: entries.map(fileRef) } });
    const link = h("a", { href: `/api/library/archive/${encodeURIComponent(ticket.id)}`, download: ticket.file_name, hidden: true });
    document.body.append(link);
    link.click();
    link.remove();
    toast(`Saving ${plural(ticket.files, "file").toLowerCase()} (${formatBytes(ticket.bytes)}) as ${ticket.file_name}`
      + (ticket.missing ? `; ${ticket.missing} no longer in the library` : ""));
  });
}

async function bulkSend(btn) {
  let joined;
  try {
    joined = (await api("/api/rooms")).filter((r) => r.joined);
  } catch (err) {
    toast(err.message, true);
    return;
  }
  if (!joined.length) {
    toast("Join a room in [5] MESSAGES first, then send tracks to it.", true);
    return;
  }
  const entries = selectedEntries();
  const choices = joined.map((room) => ({
    id: room.id,
    label: `#${room.name}`,
    detail: room.topic || `${room.members} member${room.members === 1 ? "" : "s"}`,
  }));
  openFormatMenu(btn, (roomId) => bulkBusy(async () => {
    const room = joined.find((r) => r.id === roomId);
    const sent = await api(`/api/rooms/${roomId}/tracks/bulk`, {
      method: "POST", body: { files: entries.map(fileRef) },
    });
    toast(`Sent ${plural(sent.length, "track").toLowerCase()} to #${room ? room.name : "room"}`);
  }), { title: `SEND ${plural(entries.length, "TRACK")} TO ROOM:`, choices });
}

/**
 * Convert the ticked tracks to one format, or look for lossless copies of
 * them. Tracks that cannot (already in that format, same codec, from
 * Soulseek…) are skipped and counted rather than failing the rest.
 */
function bulkConvert(btn) {
  const entries = selectedEntries();
  const able = (output) => entries.filter((entry) =>
    (entry.convert_to || []).some((o) => o.output === output && o.available));
  const upgradable = entries.filter((entry) => entry.upgrade && entry.upgrade.available);
  const of = (n) => `${n} of ${entries.length} selected`;
  // When no ticked track can, say why as the row menu would (the first
  // track's reason; they are usually all the same).
  const why = (note) => `Not possible${note ? `: ${note}` : " for any selected track"}`;
  const choices = OUTPUTS
    .filter((o) => entries.some((entry) => (entry.convert_to || []).some((c) => c.output === o.id)))
    .map((o) => {
      const n = able(o.id).length;
      const first = entries.flatMap((entry) => entry.convert_to || []).find((c) => c.output === o.id);
      return { id: o.id, label: o.label, detail: n ? `${of(n)}. ${o.detail}` : why(first && first.note), disabled: !n };
    });
  if (entries.some((entry) => entry.upgrade)) {
    choices.unshift({
      id: UPGRADE_CHOICE,
      label: "Quality upgrade",
      detail: upgradable.length
        ? `${of(upgradable.length)}: look for a lossless copy on Soulseek`
        : why((entries.find((entry) => entry.upgrade) || {}).upgrade.note),
      disabled: !upgradable.length,
    });
  }
  if (!choices.length) {
    toast("None of the selected tracks can be converted", true);
    return;
  }
  openFormatMenu(btn, (choice) => bulkBusy(async () => {
    const upgrade = choice === UPGRADE_CHOICE;
    const targets = upgrade ? upgradable : able(choice);
    let queued = 0, added = 0;
    const failed = [];
    for (const [i, entry] of targets.entries()) {
      const count = $("#lib-bulk .bulk-count");
      if (count) count.textContent = `${upgrade ? "QUEUEING UPGRADES" : "QUEUEING CONVERSIONS"} ${i + 1}/${targets.length}…`;
      try {
        const res = await api(upgrade ? "/api/library/upgrade" : "/api/library/convert", {
          method: "POST",
          body: upgrade
            ? { provider: entry.key.provider, id: entry.key.id, from: entry.output }
            : { provider: entry.key.provider, id: entry.key.id, from: entry.output, to: choice },
        });
        if (res.job_ids.length) queued += 1; else added += 1;
      } catch (err) {
        failed.push(`“${entry.title}”: ${err.message}`);
      }
    }
    const skipped = entries.length - targets.length;
    const tracks = plural(queued, "track").toLowerCase();
    const parts = [];
    if (queued) {
      parts.push(upgrade
        ? `Looking for lossless copies of ${tracks} on Soulseek; progress in Downloads`
        : `Converting ${tracks} to ${OUTPUT_SHORT[choice] || choice}; progress in Downloads`);
    }
    if (added) parts.push(`${added} already on disk, added to your library`);
    if (skipped) parts.push(`${skipped} skipped (not possible for them)`);
    if (failed.length) parts.push(`${failed.length} failed: ${failed[0]}${failed.length > 1 ? " …" : ""}`);
    toast(parts.join(" · ") || "Nothing to do", failed.length > 0 && !queued && !added);
    if (queued) await pollJobs();
    if (added) await loadLibrary();
  }), { title: `CONVERT ${plural(entries.length, "TRACK")} TO:`, choices });
}

async function bulkRemove() {
  const entries = selectedEntries();
  const everyone = isCaptain() && state.libraryScope === "all";
  const n = entries.length;
  const ok = await askDialog({
    title: `DELETE ${plural(n, "TRACK")}?`,
    text: (everyone
      ? `${n === 1 ? "Its file is" : "Their files are"} deleted from disk for the whole crew.`
      : `${n === 1 ? "It leaves" : "They leave"} your library. Files are deleted from disk unless someone else in the crew has them.`)
      + " Only the format each row shows is removed; other formats stay. This cannot be undone.",
    submit: `[ DELETE ${n} ]`,
    danger: true,
  });
  if (!ok) return;
  await bulkBusy(async () => {
    const res = await api("/api/library/remove-many", {
      method: "POST",
      body: { files: entries.map(fileRef), scope: everyone ? "all" : "mine" },
    });
    const parts = [];
    if (res.deleted) parts.push(`Deleted ${res.deleted}`);
    if (res.unlisted) parts.push(`removed ${res.unlisted} (kept for the rest of the crew)`);
    if (res.missing) parts.push(`${res.missing} already gone`);
    if (res.failed.length) parts.push(`${res.failed.length} failed: ${res.failed[0]}${res.failed.length > 1 ? " …" : ""}`);
    toast(parts.join(" · ") || "Nothing removed", res.failed.length > 0);
    state.librarySelected.clear();
    state.libraryAnchor = null;
  });
  await loadLibrary();
}

const SPECTRUM_SHORT = {
  full_band: "spectrum ok",
  lossy: "fake lossless",
  upsampled: "upsampled",
  inconclusive: "spectrum ?",
};

/** A short spectrum verdict under a file's quality, with the details on hover. */
function spectrumLine(quality) {
  const check = quality && quality.spectrum;
  if (!check) return null;
  const bad = check.verdict === "lossy" || check.verdict === "upsampled";
  return h("div", { class: bad ? "spectrum bad" : "spectrum", title: `Spectrum check: ${check.summary}` },
    SPECTRUM_SHORT[check.verdict] || check.verdict);
}

/** Hover text of a library row's quality: how the file came to be. */
function qualityTitle(entry) {
  const parts = [entry.quality.converted_from ? `Converted from ${entry.quality.converted_from}` : "Not re-encoded"];
  if (entry.quality.spectrum) parts.push(`Spectrum check: ${entry.quality.spectrum.summary}`);
  if (entry.obtained_from) {
    const from = entry.obtained_from.replace(/^slsk:\/\//, "");
    const [user, ...path] = from.split("/");
    parts.push(`Quality upgrade: ${path.join("/")} from Soulseek user ${user}`);
  }
  return parts.join(" · ");
}

/**
 * Button removing this format of the track. In the captain's whole-hold
 * view it deletes the file for everyone; otherwise it leaves the user's
 * library, and the server deletes the file only if nobody else has it.
 */
function removeButton(entry, formats) {
  const everyone = isCaptain() && state.libraryScope === "all";
  const format = OUTPUT_SHORT[entry.output] || entry.output;
  const btn = h("button", {
    class: "icon-btn danger", type: "button",
    title: everyone ? "Delete from the hold for everyone…" : "Remove from your library…",
    "aria-label": `Delete ${entry.title} (${format})`,
  }, icon("remove"));
  btn.addEventListener("click", async () => {
    const others = formats > 1 ? ` Its other ${formats - 1 === 1 ? "format stays" : "formats stay"}.` : "";
    const ok = await askDialog({
      title: `DELETE “${entry.title}” (${format})?`,
      text: (everyone
        ? `The file ${entry.path} is deleted from disk for the whole crew.`
        : `It leaves your library. The file ${entry.path} is deleted from disk unless someone else in the crew has it.`)
        + others + " This cannot be undone.",
      submit: "[ DELETE ]",
      danger: true,
    });
    if (!ok) return;
    btn.disabled = true;
    try {
      const { removal } = await api("/api/library/remove", {
        method: "POST",
        body: {
          provider: entry.key.provider, id: entry.key.id, output: entry.output,
          scope: everyone ? "all" : "mine",
        },
      });
      toast(removal === "deleted"
        ? `Deleted “${entry.title}” (${format})`
        : `Removed “${entry.title}” (${format}); the file stays for the rest of the crew`);
    } catch (err) {
      toast(err.message, true);
      btn.disabled = false;
      return;
    }
    await loadLibrary();
  });
  return btn;
}

/** Menu choice id of the quality upgrade, never a format id. */
const UPGRADE_CHOICE = "upgrade";

/**
 * Button offering the other formats this library track can be had in,
 * and (for tracks from search) a quality upgrade from Soulseek.
 */
function convertButton(entry) {
  const options = entry.convert_to || [];
  const upgrade = entry.upgrade || null;
  const btn = h("button", {
    class: "icon-btn", type: "button",
    title: upgrade ? "Convert or upgrade…" : "Convert to another format…",
    "aria-label": `Convert ${entry.title} to another format`,
    "aria-haspopup": "menu", "aria-expanded": "false",
    disabled: options.some((o) => o.available) || (upgrade && upgrade.available) ? null : true,
  }, icon("convert"));
  btn.addEventListener("click", () => {
    const choices = options.map((option) => {
      const known = OUTPUTS.find((o) => o.id === option.output);
      return {
        id: option.output,
        label: known ? known.label : option.output,
        detail: option.note
          ? `${option.available ? "" : "Not possible: "}${option.note}`
          : (known ? known.detail : ""),
        disabled: !option.available,
      };
    });
    if (upgrade) {
      choices.unshift({
        id: UPGRADE_CHOICE,
        label: "Quality upgrade",
        detail: `${upgrade.available ? "" : "Not possible: "}${upgrade.note || ""}`,
        disabled: !upgrade.available,
      });
    }
    openFormatMenu(btn, async (choice) => {
      btn.disabled = true;
      try {
        if (choice === UPGRADE_CHOICE) {
          const res = await api("/api/library/upgrade", {
            method: "POST",
            body: { provider: entry.key.provider, id: entry.key.id, from: entry.output },
          });
          if (res.job_ids.length) {
            toast(`Looking for a lossless copy of “${entry.title}” on Soulseek; progress in Downloads`);
            await pollJobs();
          } else {
            toast(res.warning || `Added a lossless copy of “${entry.title}” to your library`);
            await loadLibrary();
          }
          return;
        }
        await api("/api/library/convert", {
          method: "POST",
          body: { provider: entry.key.provider, id: entry.key.id, from: entry.output, to: choice },
        });
        toast(`Converting “${entry.title}” to ${OUTPUT_SHORT[choice] || choice}; progress in Downloads`);
        await pollJobs();
      } catch (err) {
        toast(err.message, true);
        btn.disabled = false;
      }
    }, { title: `CONVERT FROM ${OUTPUT_SHORT[entry.output] || entry.output} TO:`, choices });
  });
  return btn;
}

/** Button sending this library track to one of the user's chat rooms. */
function shareButton(entry) {
  const btn = h("button", {
    class: "icon-btn", type: "button",
    title: "Send to a chat room…", "aria-label": `Send ${entry.title} to a chat room`,
    "aria-haspopup": "menu", "aria-expanded": "false",
  }, icon("share"));
  btn.addEventListener("click", async () => {
    let joined;
    try {
      joined = (await api("/api/rooms")).filter((r) => r.joined);
    } catch (err) {
      toast(err.message, true);
      return;
    }
    if (!joined.length) {
      toast("Join a room in [5] MESSAGES first, then send tracks to it.", true);
      return;
    }
    const choices = joined.map((room) => ({
      id: room.id,
      label: `#${room.name}`,
      detail: room.topic || `${room.members} member${room.members === 1 ? "" : "s"}`,
    }));
    openFormatMenu(btn, async (roomId) => {
      const room = joined.find((r) => r.id === roomId);
      try {
        await api(`/api/rooms/${roomId}/tracks`, {
          method: "POST",
          body: { provider: entry.key.provider, id: entry.key.id, output: entry.output },
        });
        toast(`Sent “${entry.title}” to #${room ? room.name : "room"}`);
      } catch (err) {
        toast(err.message, true);
      }
    }, { title: "SEND TO ROOM:", choices });
  });
  return btn;
}

// ---------- player ----------

// A playable item: what to show while it plays, and the request that
// prepares its stream. Tracks already in the library play from the file;
// everything else streams from the provider without being downloaded.

/** Library file of a finished job, as a stream request, or null. */
function libraryRequestForJob(job) {
  if (!job || !TERMINAL.has(job.state) || !job.destination || !job.track_key) return null;
  return { source: "library", provider: job.track_key.provider, id: job.track_key.id, output: job.output };
}

function itemForTrack(track) {
  return {
    key: `src:${track.url}`,
    title: track.title,
    artists: track.artists,
    artwork: track.thumbnail_url,
    duration: track.duration_secs,
    request: libraryRequestForJob(jobForUrl(track.url)) || { source: "provider", url: track.url },
  };
}

function itemForJob(job) {
  return {
    key: `src:${job.source_url}`,
    title: job.title,
    artists: job.artists,
    artwork: job.thumbnail_url,
    duration: job.metadata && job.metadata.duration_secs,
    request: libraryRequestForJob(job) || { source: "provider", url: job.source_url },
  };
}

function itemForEntry(entry) {
  return {
    key: `src:${entry.source_url}`,
    title: entry.title,
    artists: entry.artists,
    artwork: entry.artwork_url,
    duration: null,
    request: { source: "library", provider: entry.key.provider, id: entry.key.id, output: entry.output },
  };
}

const player = {
  audio: null,
  /** { item, prepared, offset } for the loaded track. */
  current: null,
  /** Item being prepared, before it becomes `current`. */
  loading: null,
  loadToken: 0,
  /** Track position (s) the user is dragging the waveform to, or null. */
  scrub: null,
  retried: false,
  wave: {
    /** Loudness bars (0–255) measured from the playing track, or null. */
    bars: null,
    /** Length the bars span; bar i of n starts at i / n of it. */
    duration: null,
    /** How far measuring has got (s); bars after it are not measured yet. */
    measured: 0,
    /** "idle" | "loading" | "ready" | "failed" */
    status: "idle",
    abort: null,
    /** Pointer position over the waveform as a 0–1 fraction, or null. */
    hover: null,
    frame: 0,
  },
};

function playableArt(item, url, cls = "art") {
  const btn = h("button", {
    type: "button", class: "art-play",
    "data-play-key": item.key,
    "data-play-title": item.title,
    onclick: () => playItem(item),
  }, art(url, cls), h("span", { class: "art-play-icon" }));
  updatePlayButton(btn);
  return btn;
}

function isPlaying() {
  return Boolean(player.current) && !player.audio.paused && !player.audio.ended;
}

function updatePlayButton(btn) {
  const key = btn.dataset.playKey;
  const loading = Boolean(player.loading) && player.loading.key === key;
  const current = loading || (Boolean(player.current) && player.current.item.key === key);
  const playing = current && !loading && isPlaying();
  btn.classList.toggle("current", current);
  btn.classList.toggle("loading", loading);
  btn.setAttribute("aria-label", `${playing ? "Pause" : "Play"} ${btn.dataset.playTitle}`);
  const slot = btn.querySelector(".art-play-icon");
  slot.replaceChildren(icon(playing ? "pause" : "play"));
}

function syncPlayButtons() {
  for (const btn of document.querySelectorAll(".art-play")) updatePlayButton(btn);
  const toggle = $("#play-toggle");
  const playing = isPlaying();
  toggle.classList.toggle("playing", playing);
  toggle.setAttribute("aria-label", playing ? "Pause" : "Play");
  toggle.disabled = !player.current && !player.loading;
  toggle.classList.toggle("loading", Boolean(player.loading));
  syncMascot(playing);
  if ("mediaSession" in navigator) {
    navigator.mediaSession.playbackState = player.current ? (playing ? "playing" : "paused") : "none";
  }
}

/** How long playback must stay stopped before Kult lies down. */
const MASCOT_SLEEP_DELAY_MS = 350;
/**
 * Kult's dance moves and how many beats each lasts. A beat is 0.5s (120 BPM),
 * matching the keyframes in app.css; a move's animations loop in whole beats,
 * so cutting to the next one always lands on the beat. Tricks (spin,
 * backflip) are one-offs: they are never picked twice in a row.
 */
const KULT_BEAT_MS = 500;
const KULT_MOVES = {
  groove: 8, disco: 8, robot: 8, headbang: 8, shuffle: 8, moonwalk: 8, spin: 4, backflip: 4,
};
const mascot = { dancing: false, timer: 0, move: "groove", routine: 0 };

/**
 * The audio element pauses briefly whenever playback is rearranged: playItem
 * pauses it while the next stream is prepared, and assigning a new src (track
 * change, restart-to-seek, stream recovery) fires "pause" before play resumes.
 * Keep dancing through those, and only fall asleep once playback has really
 * stopped for a moment.
 */
function syncMascot(playing) {
  clearTimeout(mascot.timer);
  const keepDancing = playing || (mascot.dancing && Boolean(player.loading));
  if (keepDancing) {
    setMascotDancing(true);
  } else if (mascot.dancing) {
    mascot.timer = setTimeout(() => setMascotDancing(false), MASCOT_SLEEP_DELAY_MS);
  }
}

function setMascotDancing(dancing) {
  mascot.dancing = dancing;
  for (const kult of document.querySelectorAll(".kult")) kult.classList.toggle("playing", dancing);
  syncKultRoutine();
}

/** Run the move sequencer only while Kult is on stage and dancing. */
function syncKultRoutine() {
  const run = mascot.dancing && kultEnabled();
  if (run && !mascot.routine) {
    mascot.routine = setTimeout(nextKultMove, KULT_MOVES[mascot.move] * KULT_BEAT_MS);
  } else if (!run && mascot.routine) {
    // The current move freezes mid-step (see app.css) and resumes on waking.
    clearTimeout(mascot.routine);
    mascot.routine = 0;
  }
}

function nextKultMove() {
  const moves = Object.keys(KULT_MOVES).filter((m) => m !== mascot.move);
  mascot.move = moves[Math.floor(Math.random() * moves.length)];
  for (const kult of document.querySelectorAll(".kult")) kult.dataset.move = mascot.move;
  mascot.routine = 0;
  syncKultRoutine();
}

/** The header's Kult is a copy of the top bar's, so the drawing lives in one place. */
function setupKult() {
  const svg = $("#appbar-brand .skelly");
  if (svg) $("#head-kult").append(svg.cloneNode(true));
}

function streamUrl(prepared, start) {
  return start > 0 ? `/api/stream/${prepared.id}?start=${start.toFixed(1)}` : `/api/stream/${prepared.id}`;
}

/** Playback position in the track, including the offset of restarted streams. */
function position() {
  if (!player.current) return 0;
  return player.current.offset + (player.audio.currentTime || 0);
}

function trackDuration() {
  const cur = player.current;
  if (!cur) return null;
  if (cur.prepared.seekable && isFinite(player.audio.duration)) return player.audio.duration;
  return cur.prepared.duration_secs || cur.item.duration || null;
}

function showNowPlaying(item, sourceText) {
  const img = $("#np-art");
  if (typeof item.artwork === "string" && item.artwork.startsWith("https://")) img.src = item.artwork;
  else img.removeAttribute("src");
  $("#np-title").textContent = item.title;
  $("#np-title").title = item.title;
  $("#np-artist").textContent = item.artists.join(", ") || "Unknown artist";
  $("#np-source").textContent = sourceText;
  // Nothing loaded: on compact screens the empty player is hidden (app.css).
  $("#player").classList.toggle("idle", !player.current && !player.loading);
}

function describeSource(prepared) {
  const where = prepared.origin === "library"
    ? "From your library"
    : `Streaming from ${PROVIDER_NAMES[prepared.provider] || prepared.provider}`;
  return `${where} · ${prepared.summary}`;
}

async function playItem(item) {
  if (player.current && player.current.item.key === item.key && !player.loading) {
    togglePlay();
    return;
  }
  const token = ++player.loadToken;
  player.loading = item;
  player.audio.pause();
  showNowPlaying(item, "Preparing stream…");
  syncPlayButtons();
  try {
    const prepared = await api("/api/stream", { method: "POST", body: item.request });
    if (token !== player.loadToken) return;
    player.current = { item, prepared, offset: 0 };
    player.loading = null;
    player.retried = false;
    loadWaveform(prepared);
    showNowPlaying(item, describeSource(prepared));
    setMediaSession(item);
    player.audio.src = streamUrl(prepared, 0);
    startPlayback();
  } catch (err) {
    if (token !== player.loadToken) return;
    player.loading = null;
    if (player.current) {
      showNowPlaying(player.current.item, describeSource(player.current.prepared));
    } else {
      showNowPlaying({ title: "NOTHING PLAYING", artists: [], artwork: null }, "");
      $("#np-artist").textContent = "PRESS [>] ON ANY TRACK TO LISTEN";
    }
    toast(`Cannot play “${item.title}”: ${err.message}`, true);
  }
  syncPlayButtons();
  updateTime();
}

function startPlayback() {
  player.audio.play().catch((err) => {
    // Browsers may block playback started long after the click (slow
    // stream preparation); the play button then starts it.
    if (err.name === "NotAllowedError") toast("Press play to start listening");
  });
}

function togglePlay() {
  if (!player.current) return;
  if (player.audio.paused || player.audio.ended) startPlayback();
  else player.audio.pause();
}

function seekTo(target) {
  const cur = player.current;
  if (!cur) return;
  const duration = trackDuration();
  const t = Math.max(0, duration ? Math.min(target, duration - 0.5) : target);
  if (cur.prepared.seekable) {
    player.audio.currentTime = t;
  } else {
    // ffmpeg-backed streams have no byte ranges: restart them at `t`.
    const resume = !player.audio.paused;
    cur.offset = t;
    player.audio.src = streamUrl(cur.prepared, t);
    if (resume) startPlayback();
  }
  updateTime();
}

function updateTime() {
  const seek = $("#seek");
  const duration = trackDuration();
  const canSeek = Boolean(player.current) && Boolean(duration);
  seek.classList.toggle("disabled", !canSeek);
  seek.setAttribute("aria-disabled", String(!canSeek));
  seek.tabIndex = canSeek ? 0 : -1;
  const shown = player.scrub ?? Math.min(position(), duration || 0);
  seek.setAttribute("aria-valuemax", String(Math.round(duration || 0)));
  seek.setAttribute("aria-valuenow", String(Math.round(shown)));
  $("#time-current").textContent = player.current ? formatTime(shown) : "0:00";
  $("#time-total").textContent = player.current ? formatTime(duration) : "0:00";
  seek.setAttribute("aria-valuetext", `${$("#time-current").textContent} of ${$("#time-total").textContent}`);
  drawWave();
}

// ---------- waveform ----------
//
// The seek bar is the track's waveform, measured by the server from the
// decoded audio (/api/stream/{id}/waveform), so its shape lines up with
// what plays. The server measures in the background; polling it returns
// the bars measured so far, laid out over the expected length, so the
// waveform fills in from the left. Unmeasured parts are a flat line, and
// seeking works throughout.

const WAVE_POLL_MS = 250;

/** Length the waveform maps onto: the measured audio once known. */
function waveDuration() {
  const { wave } = player;
  return wave.bars && wave.duration ? wave.duration : trackDuration();
}

async function loadWaveform(prepared) {
  const wave = player.wave;
  if (wave.abort) wave.abort.abort();
  const abort = new AbortController();
  Object.assign(wave, { bars: null, duration: null, measured: 0, status: "loading", abort });
  drawWave();
  try {
    for (;;) {
      const data = await api(`/api/stream/${prepared.id}/waveform`, { signal: abort.signal });
      if (wave.abort !== abort) return;
      const valid = data && Array.isArray(data.bars) && typeof data.duration_secs === "number";
      if (!valid) throw new Error("malformed waveform");
      if (data.bars.length > 0 && data.duration_secs > 0) {
        Object.assign(wave, {
          bars: Uint8Array.from(data.bars),
          duration: data.duration_secs,
          measured: data.complete ? data.duration_secs : data.measured_secs,
        });
        updateTime();
      }
      if (data.complete) {
        if (!wave.bars) throw new Error("no audio to draw");
        wave.status = "ready";
        break;
      }
      await new Promise((resolve) => setTimeout(resolve, WAVE_POLL_MS));
      if (wave.abort !== abort) return;
    }
  } catch (err) {
    if (wave.abort !== abort) return;
    // Playback works without it; the flat line stays and stays seekable.
    Object.assign(wave, { bars: null, duration: null, measured: 0, status: "failed" });
    console.warn("waveform unavailable:", err.message);
  }
  wave.abort = null;
  updateTime();
}

function waveColors() {
  const css = getComputedStyle(document.documentElement);
  const get = (name) => css.getPropertyValue(name).trim();
  return { played: get("--fg"), ahead: get("--dim"), preview: get("--mid"), empty: get("--faint") };
}

/** Redraw on the next frame; keeps going every frame while playing. */
function drawWave() {
  if (player.wave.frame) return;
  player.wave.frame = requestAnimationFrame(() => {
    player.wave.frame = 0;
    paintWave();
    if (isPlaying()) drawWave();
  });
}

function paintWave() {
  const canvas = $("#wave-canvas");
  const dpr = window.devicePixelRatio || 1;
  const width = Math.round(canvas.clientWidth * dpr);
  const height = Math.round(canvas.clientHeight * dpr);
  if (!width || !height) return;
  if (canvas.width !== width || canvas.height !== height) {
    canvas.width = width;
    canvas.height = height;
  }
  const ctx = canvas.getContext("2d");
  ctx.clearRect(0, 0, width, height);

  // SoundCloud-style: bars rise from a baseline at 70% height, with a
  // dimmer reflection below it. Sizes snap to device pixels to stay crisp.
  const barW = Math.max(1, Math.round(2 * dpr));
  const step = barW + Math.max(1, Math.round(dpr));
  const count = Math.max(1, Math.floor((width + step - barW) / step));
  const base = Math.round(height * 0.7);
  const up = base - 1;
  const down = height - base - 1;

  const { wave } = player;
  const duration = waveDuration();
  const pos = player.current ? (player.scrub ?? position()) : 0;
  const played = duration ? Math.min(1, pos / duration) : 0;
  const hover = player.current && duration ? wave.hover : null;
  const colors = waveColors();
  const bars = wave.bars;
  const measured = duration && bars ? wave.measured / duration : 0;

  for (let i = 0; i < count; i++) {
    const from = i / count;
    let level = 0;
    if (bars && from < measured) {
      // Loudest of the measured bars this drawn bar covers.
      const a = Math.floor(from * bars.length);
      const b = Math.max(a + 1, Math.floor(((i + 1) / count) * bars.length));
      for (let j = a; j < b && j < bars.length; j++) level = Math.max(level, bars[j]);
      level /= 255;
    }
    const h = Math.max(1, Math.round(level * up));
    const center = (i + 0.5) / count;
    let color = colors.ahead;
    if (!player.current) color = colors.empty;
    else if (center <= played) color = colors.played;
    // Between the playhead and the pointer: where a click would jump to.
    if (hover !== null && center > Math.min(played, hover) && center <= Math.max(played, hover)) {
      color = colors.preview;
    }
    const x = i * step;
    ctx.fillStyle = color;
    ctx.globalAlpha = 1;
    ctx.fillRect(x, base - h, barW, h);
    ctx.globalAlpha = 0.35;
    ctx.fillRect(x, base + 1, barW, Math.max(1, Math.round(level * down)));
  }
  ctx.globalAlpha = 1;
  // Pulses only until the first bars arrive; then they fill in instead.
  $("#seek").classList.toggle("loading", wave.status === "loading" && !bars);
}

function setupWaveform() {
  const seek = $("#seek");
  const tip = $("#wave-tip");
  const enabled = () => !seek.classList.contains("disabled");
  const fractionAt = (event) => {
    const rect = seek.getBoundingClientRect();
    return rect.width ? Math.min(1, Math.max(0, (event.clientX - rect.left) / rect.width)) : 0;
  };
  const showTip = (fraction) => {
    const duration = waveDuration();
    if (fraction === null || !duration) {
      tip.hidden = true;
      return;
    }
    tip.hidden = false;
    tip.textContent = formatTime(fraction * duration);
    tip.style.left = `${fraction * 100}%`;
  };

  seek.addEventListener("pointermove", (event) => {
    if (!enabled()) return;
    const fraction = fractionAt(event);
    // Touch has no hover; only show the preview while dragging.
    player.wave.hover = event.pointerType === "mouse" || player.scrub !== null ? fraction : null;
    if (player.scrub !== null) player.scrub = fraction * waveDuration();
    showTip(player.wave.hover);
    updateTime();
  });
  seek.addEventListener("pointerleave", () => {
    if (player.scrub !== null) return;
    player.wave.hover = null;
    showTip(null);
    drawWave();
  });
  seek.addEventListener("pointerdown", (event) => {
    if (!enabled() || event.button !== 0) return;
    seek.setPointerCapture(event.pointerId);
    const fraction = fractionAt(event);
    player.scrub = fraction * waveDuration();
    player.wave.hover = fraction;
    showTip(fraction);
    updateTime();
  });
  const release = (commit) => (event) => {
    if (player.scrub === null) return;
    const target = player.scrub;
    player.scrub = null;
    if (event.pointerType !== "mouse") {
      player.wave.hover = null;
      showTip(null);
    }
    if (commit) seekTo(target);
    else updateTime();
  };
  seek.addEventListener("pointerup", release(true));
  seek.addEventListener("pointercancel", release(false));

  seek.addEventListener("keydown", (event) => {
    if (!enabled()) return;
    const duration = waveDuration();
    const now = position();
    const targets = {
      ArrowLeft: now - 5, ArrowDown: now - 5, ArrowRight: now + 5, ArrowUp: now + 5,
      PageDown: now - 30, PageUp: now + 30, Home: 0, End: duration,
    };
    if (!(event.key in targets)) return;
    event.preventDefault();
    seekTo(targets[event.key]);
  });

  new ResizeObserver(() => drawWave()).observe(seek);
  // Theme switches change the bar colours.
  new MutationObserver(() => drawWave())
    .observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme"] });
  drawWave();
}

// The player is fixed over the bottom of the page and grows once a track is
// loaded (cover art, title, artist, source lines), so the page reserves its
// measured height rather than the --player-h estimate. Otherwise the chat
// input, toasts and the end of long lists slide underneath it.
function trackPlayerHeight() {
  const bar = $("#player");
  const root = document.documentElement;
  // Folded, the player is a tab floating above the bottom edge, so the
  // reserved space is its height plus that gap; hidden, it is nothing.
  const apply = () => {
    const height = bar.getBoundingClientRect().height;
    const gap = height ? parseFloat(getComputedStyle(bar).bottom) || 0 : 0;
    root.style.setProperty("--player-space", `${Math.ceil(height + gap)}px`);
  };
  new ResizeObserver(apply).observe(bar, { box: "border-box" });
  apply();
}

const PLAYER_FOLD_KEY = "inkult.playerFolded";

/** Fold the player into a small tab (title, play/pause) or open it again. */
function setPlayerFolded(folded, { remember = true } = {}) {
  const bar = $("#player");
  if (bar.classList.contains("folded") === folded) return;
  bar.classList.toggle("folded", folded);
  // Unfolding animates the full bar back in; folding animates the tab (CSS).
  bar.classList.toggle("unfolding", !folded && remember);
  const btn = $("#player-fold");
  const label = folded ? "Show player" : "Hide player";
  btn.setAttribute("aria-expanded", String(!folded));
  btn.setAttribute("aria-label", label);
  btn.title = label;
  btn.replaceChildren(h("span", { "aria-hidden": "true" }, folded ? "^" : "v"));
  if (remember) {
    try { localStorage.setItem(PLAYER_FOLD_KEY, folded ? "1" : "0"); } catch { /* storage unavailable */ }
  }
}

function setupPlayerFold() {
  const bar = $("#player");
  let stored = false;
  try { stored = localStorage.getItem(PLAYER_FOLD_KEY) === "1"; } catch { /* storage unavailable */ }
  setPlayerFolded(stored, { remember: false });
  $("#player-fold").addEventListener("click", () => setPlayerFolded(!bar.classList.contains("folded")));
  bar.addEventListener("animationend", () => bar.classList.remove("unfolding"));
  // Tapping the title in the folded tab opens the player again.
  const np = bar.querySelector(".np");
  np.addEventListener("click", () => { if (bar.classList.contains("folded")) setPlayerFolded(false); });
  // Swiping the open player down folds it, like dismissing a sheet. Only the
  // track info area takes the gesture; the waveform keeps its scrubbing.
  let startY = null;
  np.addEventListener("touchstart", (event) => {
    startY = event.touches.length === 1 && !bar.classList.contains("folded") ? event.touches[0].clientY : null;
  }, { passive: true });
  np.addEventListener("touchend", (event) => {
    if (startY !== null && event.changedTouches[0].clientY - startY > 40) setPlayerFolded(true);
    startY = null;
  });
}

function setMediaSession(item) {
  if (!("mediaSession" in navigator) || typeof MediaMetadata === "undefined") return;
  const artwork = typeof item.artwork === "string" && item.artwork.startsWith("https://")
    ? [{ src: item.artwork }] : [];
  navigator.mediaSession.metadata = new MediaMetadata({
    title: item.title, artist: item.artists.join(", "), artwork,
  });
}

function storedVolume() {
  try {
    const v = Number(localStorage.getItem(VOLUME_KEY));
    return localStorage.getItem(VOLUME_KEY) !== null && v >= 0 && v <= 1 ? v : 0.8;
  } catch {
    return 0.8;
  }
}

function updateVolumeUi() {
  const { audio } = player;
  const level = audio.muted ? 0 : audio.volume;
  $("#volume").value = String(level);
  const mute = $("#mute");
  mute.replaceChildren(icon(level === 0 ? "volumeOff" : level < 0.5 ? "volumeLow" : "volumeHigh"));
  mute.setAttribute("aria-label", audio.muted ? "Unmute" : "Mute");
}

/**
 * Why the stream at `url` failed. The audio element only reports a generic
 * media error, so ask the server again and read its JSON error; a stream
 * that opens fine now means the browser could not play what it got.
 */
async function streamFailureReason(url) {
  const fallback = player.audio.error && player.audio.error.message;
  const abort = new AbortController();
  try {
    const res = await fetch(url, { credentials: "same-origin", signal: abort.signal });
    if (!res.ok) {
      const data = await res.json().catch(() => null);
      return (data && data.error) || `server answered ${res.status}`;
    }
  } catch {
    /* network error: nothing more specific to say */
  } finally {
    abort.abort();
  }
  return fallback || null;
}

/** A stream failed mid-way (e.g. it expired): prepare it again once and resume. */
async function recoverStream() {
  const cur = player.current;
  if (!cur || player.retried || player.loading) {
    const failedUrl = player.audio.getAttribute("src");
    syncPlayButtons();
    if (!cur) {
      toast("Playback failed", true);
      return;
    }
    const reason = failedUrl ? await streamFailureReason(failedUrl) : null;
    toast(`Playback of “${cur.item.title}” failed${reason ? `: ${reason}` : ""}`, true);
    return;
  }
  player.retried = true;
  const at = position();
  try {
    const prepared = await api("/api/stream", { method: "POST", body: cur.item.request });
    if (player.current !== cur) return;
    cur.prepared = prepared;
    cur.offset = prepared.seekable ? 0 : at;
    player.audio.src = streamUrl(prepared, cur.offset);
    if (prepared.seekable) {
      player.audio.addEventListener("loadedmetadata", () => { player.audio.currentTime = at; }, { once: true });
    }
    startPlayback();
  } catch (err) {
    toast(`Playback of “${cur.item.title}” failed: ${err.message}`, true);
  }
}

function setupPlayer() {
  const audio = $("#audio");
  player.audio = audio;
  audio.volume = storedVolume();
  updateVolumeUi();
  syncPlayButtons();

  for (const event of ["play", "pause", "playing", "ended"]) audio.addEventListener(event, syncPlayButtons);
  for (const event of ["timeupdate", "durationchange", "loadedmetadata"]) audio.addEventListener(event, updateTime);
  audio.addEventListener("playing", () => { player.retried = false; });
  audio.addEventListener("ended", () => {
    if (!player.current) return;
    player.current.offset = 0;
    if (!player.current.prepared.seekable) {
      // Rewind without starting: a fresh stream from the beginning.
      audio.src = streamUrl(player.current.prepared, 0);
    }
    updateTime();
  });
  audio.addEventListener("error", () => {
    if (player.current && audio.getAttribute("src")) recoverStream();
  });

  $("#play-toggle").addEventListener("click", togglePlay);
  setupWaveform();
  setupPlayerFold();
  trackPlayerHeight();
  $("#volume").addEventListener("input", (event) => {
    audio.volume = Number(event.target.value);
    audio.muted = audio.volume === 0;
    try { localStorage.setItem(VOLUME_KEY, String(audio.volume)); } catch { /* storage unavailable */ }
  });
  $("#mute").addEventListener("click", () => {
    if (audio.muted || audio.volume === 0) {
      audio.muted = false;
      if (audio.volume === 0) audio.volume = 0.5;
    } else {
      audio.muted = true;
    }
  });
  audio.addEventListener("volumechange", updateVolumeUi);

  // Space toggles playback unless typing or operating another control.
  document.addEventListener("keydown", (event) => {
    if (event.code !== "Space" || event.repeat || event.ctrlKey || event.metaKey || event.altKey) return;
    const target = event.target;
    if (target instanceof Element && target.closest("input, textarea, select, button, a, [contenteditable]")) return;
    if (!player.current) return;
    event.preventDefault();
    togglePlay();
  });

  if ("mediaSession" in navigator) {
    const actions = {
      play: () => startPlayback(),
      pause: () => audio.pause(),
      seekto: (details) => seekTo(details.seekTime),
    };
    for (const [action, handler] of Object.entries(actions)) {
      try { navigator.mediaSession.setActionHandler(action, handler); } catch { /* unsupported */ }
    }
  }
}

// ---------- soulseek ----------

// Everything shown here comes from other Soulseek users: file names, user
// names, messages and descriptions are untrusted and only ever inserted as
// text. User pictures arrive as validated data: URLs from the server.

const SLSK_AUDIO = new Set(["flac", "mp3", "m4a", "aac", "ogg", "oga", "opus", "wav", "aif", "aiff"]);
const SLSK_LOSSLESS = new Set(["flac", "wav", "aif", "aiff", "alac", "ape", "wv"]);
const SLSK_STATES = {
  queued: "Queued", requesting: "Contacting", remote: "In their queue", offered: "Starting",
  transferring: "Receiving", importing: "Tagging", completed: "Done", skipped: "In library",
  failed: "Failed", cancelled: "Cancelled",
};
const SLSK_TERMINAL = new Set(["completed", "skipped", "failed", "cancelled"]);
const SLSK_UP_STATES = {
  queued: "Queued", starting: "Starting", transferring: "Sending", completed: "Sent",
  failed: "Failed", cancelled: "Cancelled",
};
/** Groups rendered at once; more on request (big searches have thousands). */
const SLSK_PAGE = 60;

const slsk = {
  status: null,
  pane: "search",
  timer: null,
  ticking: false,
  searches: [],
  /** Search shown in the results area. */
  searchId: null,
  /** id → { summary, replies } */
  results: new Map(),
  filter: { text: "", format: "all", kbps: 0, sort: "best", freeOnly: false },
  shown: SLSK_PAGE,
  downloads: [],
  uploads: [],
  /** `${user}\n${path}` of unfinished or finished downloads, for result buttons. */
  wanted: new Map(),
  browseUser: null,
  browse: null,
  browseOpen: null,
  browseFiles: null,
  browseFilter: "",
  userInfo: null,
  convos: [],
  chatUser: null,
  chat: null,
  /** Re-render the search results on the next tick (e.g. new replies). */
  dirty: false,
};

function slskSignedIn() {
  const s = slsk.status;
  return Boolean(s) && ["online", "connecting", "reconnecting"].includes(s.state);
}

function extOf(name) {
  const m = /\.([^.\\/]+)$/.exec(name || "");
  return m ? m[1].toLowerCase() : "";
}

function baseName(path) {
  const parts = String(path).split("\\");
  return parts[parts.length - 1];
}

function dirName(path) {
  const i = String(path).lastIndexOf("\\");
  return i < 0 ? "" : path.slice(0, i);
}

function formatSpeed(bytesPerSec) {
  return typeof bytesPerSec === "number" && bytesPerSec > 0 ? `${formatBytes(bytesPerSec)}/s` : "";
}

/** "FLAC 24-BIT 96 KHZ", "MP3 320", "MP3 245 VBR" from the peer's attributes. */
function slskQuality(file) {
  const ext = extOf(file.name);
  const a = file.attrs || {};
  const parts = [ext ? ext.toUpperCase() : "?"];
  if (SLSK_LOSSLESS.has(ext)) {
    if (a.bit_depth) parts.push(`${a.bit_depth}-BIT`);
    if (a.sample_rate_hz) {
      const khz = a.sample_rate_hz / 1000;
      parts.push(`${a.sample_rate_hz % 1000 ? khz.toFixed(1) : khz} KHZ`);
    }
  } else if (a.bitrate_kbps) {
    parts.push(`${a.bitrate_kbps}${a.vbr ? " VBR" : ""}`);
  }
  return parts.join(" ");
}

/** Rough ordering for "quality" sort: lossless above lossy, then resolution/bitrate. */
function slskQualityScore(file) {
  const ext = extOf(file.name);
  const a = file.attrs || {};
  if (SLSK_LOSSLESS.has(ext)) return 100000 + (a.bit_depth || 16) * 1000 + (a.sample_rate_hz || 44100) / 1000;
  const weight = ext === "opus" ? 1.5 : (ext === "m4a" || ext === "aac" || ext === "ogg") ? 1.2 : 1;
  return (a.bitrate_kbps || 0) * weight;
}

function slskFormatMatches(file, format) {
  const ext = extOf(file.name);
  switch (format) {
    case "lossless": return SLSK_LOSSLESS.has(ext);
    case "flac": return ext === "flac";
    case "mp3": return ext === "mp3";
    case "aac": return ext === "m4a" || ext === "aac";
    case "wav": return ext === "wav" || ext === "aif" || ext === "aiff";
    default: return true;
  }
}

function slskTextMatches(path, text) {
  if (!text) return true;
  const hay = path.toLowerCase();
  for (const word of text.toLowerCase().split(/\s+/).filter(Boolean)) {
    if (word.startsWith("-") && word.length > 1) {
      if (hay.includes(word.slice(1))) return false;
    } else if (!hay.includes(word)) {
      return false;
    }
  }
  return true;
}

// ---------- status & polling ----------

async function slskLoadStatus() {
  try {
    slsk.status = await api("/api/soulseek");
  } catch {
    return;
  }
  renderSlskStatus();
}

function renderSlskStatus() {
  const s = slsk.status;
  if (!s) return;
  const signedIn = slskSignedIn();
  const badge = $("#slsk-badge");
  const count = s.unread_messages || (signedIn ? s.active_downloads : 0);
  badge.hidden = !count;
  badge.textContent = String(count);
  const unread = $("#slsk-unread-badge");
  unread.hidden = !s.unread_messages;
  unread.textContent = String(s.unread_messages);
  const transfers = $("#slsk-transfers-badge");
  const active = (s.active_downloads || 0) + (s.active_uploads || 0);
  transfers.hidden = !active;
  transfers.textContent = String(active);
  if (state.view !== "soulseek") return;

  $("#slsk-signin").hidden = signedIn;
  $("#slsk-main").hidden = !signedIn;
  const conn = $("#slsk-conn");
  conn.className = `slsk-conn ${s.state}`;
  conn.textContent = {
    online: `ONLINE AS ${s.username}`,
    connecting: "CONNECTING…",
    reconnecting: s.message ? s.message.toUpperCase() : "RECONNECTING…",
    kicked: "SIGNED OUT",
    rejected: "SIGNED OUT",
    signed_out: "OFFLINE",
  }[s.state] || s.state;
  conn.classList.add("keep-case");

  if (!signedIn) {
    const user = $("#slsk-user");
    if (!user.value && s.username) user.value = s.username;
    const error = $("#slsk-signin-error");
    error.textContent = s.message || "";
    error.hidden = !s.message || !["kicked", "rejected"].includes(s.state);
  }
  if (slsk.pane === "setup") renderSlskSetup();
}

function slskSchedule(delay) {
  clearTimeout(slsk.timer);
  if (!state.user) return;
  slsk.timer = setTimeout(slskTick, delay);
}

async function slskTick() {
  if (slsk.ticking) return;
  slsk.ticking = true;
  try {
    await slskLoadStatus();
    if (state.view === "soulseek" && slskSignedIn()) await slskRefreshPane();
  } finally {
    slsk.ticking = false;
  }
  let next = 15000;
  if (state.view === "soulseek") {
    const searching = slsk.pane === "search" && slskCurrentSummary() && slskCurrentSummary().active;
    const moving = slsk.downloads.some((d) => ["transferring", "offered", "importing", "requesting"].includes(d.state));
    next = searching || (slsk.pane === "transfers" && moving) ? 1200 : 3000;
  }
  slskSchedule(next);
}

async function slskRefreshPane() {
  // Downloads are always fetched: search results show what is already queued.
  await slskLoadDownloads();
  switch (slsk.pane) {
    case "search": await slskLoadSearches(); break;
    case "transfers": await slskLoadUploads(); renderSlskTransfers(); break;
    case "browse": await slskRefreshBrowse(); break;
    case "messages": await slskLoadChats(); break;
    case "setup": renderSlskSetup(); break;
  }
}

function slskEnter() {
  renderSlskStatus();
  slskShowPane(slsk.pane);
  slskSchedule(0);
}

function slskShowPane(pane) {
  slsk.pane = pane;
  for (const chip of document.querySelectorAll("#slsk-tabs .chip")) {
    const active = chip.dataset.pane === pane;
    chip.classList.toggle("active", active);
    chip.setAttribute("aria-checked", String(active));
  }
  for (const el of document.querySelectorAll(".slsk-pane")) el.hidden = el.id !== `slsk-pane-${pane}`;
  // On phones the tabs are one swipeable row; keep the picked one in view.
  document.querySelector("#slsk-tabs .chip.active")?.scrollIntoView({ block: "nearest", inline: "nearest" });
  if (pane === "search") renderSlskResults();
  if (pane === "transfers") renderSlskTransfers();
  if (pane === "browse") renderSlskBrowse();
  if (pane === "messages") renderSlskChats();
  if (pane === "setup") renderSlskSetup();
}

// ---------- sign in ----------

async function slskSignIn(event) {
  event.preventDefault();
  const username = $("#slsk-user").value.trim();
  const password = $("#slsk-pass").value;
  const error = $("#slsk-signin-error");
  if (!username || !password) {
    error.textContent = "Enter your Soulseek username and password.";
    error.hidden = false;
    return;
  }
  const button = $("#slsk-signin-submit");
  button.disabled = true;
  button.textContent = "[ DIALING... ]";
  error.hidden = true;
  try {
    slsk.status = await api("/api/soulseek/login", {
      method: "POST",
      body: { username, password, remember: $("#slsk-remember").checked },
    });
    $("#slsk-pass").value = "";
    toast(`Connected to Soulseek as ${username}`);
    renderSlskStatus();
    slskShowPane("search");
    $("#slsk-query").focus();
    slskSchedule(0);
  } catch (err) {
    error.textContent = err.message;
    error.hidden = false;
  } finally {
    button.disabled = false;
    button.textContent = "[ CONNECT ]";
  }
}

async function slskSignOut() {
  const ok = await askDialog({
    title: "SIGN OUT OF SOULSEEK?",
    text: "Searches, conversations and running transfers end. Unfinished downloads are kept and resume at your next sign-in. The stored password is deleted.",
    submit: "[ SIGN OUT ]",
    danger: true,
  });
  if (!ok) return;
  try {
    await api("/api/soulseek/logout", { method: "POST" });
  } catch (err) {
    toast(err.message, true);
  }
  slsk.results.clear();
  slsk.searches = [];
  slsk.searchId = null;
  await slskLoadStatus();
}

// ---------- search ----------

function slskCurrentSummary() {
  return slsk.searches.find((s) => s.id === slsk.searchId) || null;
}

async function slskSearch(query, user = null) {
  try {
    const summary = await api("/api/soulseek/searches", { method: "POST", body: { query, user } });
    slsk.searches.unshift(summary);
    slsk.searchId = summary.id;
    slsk.results.set(summary.id, { summary, replies: [] });
    slsk.shown = SLSK_PAGE;
    slskShowPane("search");
    renderSlskHistory();
    renderSlskResults();
    slskSchedule(800);
  } catch (err) {
    toast(err.message, true);
  }
}

async function slskLoadSearches() {
  try {
    slsk.searches = await api("/api/soulseek/searches");
  } catch {
    return;
  }
  if (slsk.searchId != null && !slsk.searches.some((s) => s.id === slsk.searchId)) slsk.searchId = null;
  renderSlskHistory();
  const id = slsk.searchId;
  if (id == null) {
    renderSlskResults();
    return;
  }
  const entry = slsk.results.get(id) || { summary: null, replies: [] };
  try {
    const view = await api(`/api/soulseek/searches/${id}?from=${entry.replies.length}`);
    if (view.from === entry.replies.length) entry.replies.push(...view.results);
    const grew = view.results.length > 0;
    entry.summary = view;
    slsk.results.set(id, entry);
    if (grew || slsk.dirty) renderSlskResults();
    else renderSlskResultsHead();
  } catch {
    // Transient; next tick retries.
  }
}

function slskSearchLabel(summary) {
  const scope = summary.scope || {};
  if (scope.kind === "user") return `${scope.username}: ${summary.query}`;
  if (scope.kind === "wishlist") return `WISH: ${summary.query}`;
  return summary.query;
}

function renderSlskHistory() {
  const container = $("#slsk-history");
  if (!slsk.searches.length) {
    container.replaceChildren();
    return;
  }
  container.replaceChildren(h("div", { class: "chips slsk-history-list" },
    h("span", { class: "dim" }, "HISTORY:"),
    slsk.searches.map((s) => h("span", { class: "slsk-hist" },
      h("button", {
        type: "button", class: s.id === slsk.searchId ? "chip active keep-case" : "chip keep-case",
        onclick: () => {
          slsk.searchId = s.id;
          slsk.shown = SLSK_PAGE;
          renderSlskHistory();
          renderSlskResults();
          slskSchedule(0);
        },
      }, `${slskSearchLabel(s)} (${s.files})`, s.active ? h("span", { class: "blink" }, "*") : null),
      h("button", {
        type: "button", class: "icon-btn", title: "Forget this search", "aria-label": `Forget search ${s.query}`,
        onclick: async () => {
          try {
            await api(`/api/soulseek/searches/${s.id}`, { method: "DELETE" });
            slsk.results.delete(s.id);
            if (slsk.searchId === s.id) slsk.searchId = null;
            await slskLoadSearches();
            renderSlskResults();
          } catch (err) { toast(err.message, true); }
        },
      }, "X")))));
}

/** Results grouped by user and folder, filtered and sorted. */
function slskGroups(replies) {
  const f = slsk.filter;
  const groups = new Map();
  for (const reply of replies) {
    if (f.freeOnly && !reply.slot_free) continue;
    for (const file of reply.files) {
      const ext = extOf(file.name);
      if (!SLSK_AUDIO.has(ext)) continue;
      if (!slskFormatMatches(file, f.format)) continue;
      if (f.kbps && !SLSK_LOSSLESS.has(ext) && !((file.attrs && file.attrs.bitrate_kbps) >= f.kbps)) continue;
      if (!slskTextMatches(file.name, f.text)) continue;
      const folder = dirName(file.name);
      const key = `${reply.username}\n${folder}`;
      let group = groups.get(key);
      if (!group) {
        group = { key, username: reply.username, folder, slot_free: reply.slot_free,
          avg_speed: reply.avg_speed, queue_length: reply.queue_length, files: [], best: 0 };
        groups.set(key, group);
      }
      group.files.push(file);
      group.best = Math.max(group.best, slskQualityScore(file));
    }
  }
  const list = Array.from(groups.values());
  const bySpeed = (a, b) => b.avg_speed - a.avg_speed;
  const sorters = {
    best: (a, b) => (Number(b.slot_free) - Number(a.slot_free)) || (a.queue_length - b.queue_length) || bySpeed(a, b),
    quality: (a, b) => (b.best - a.best) || (Number(b.slot_free) - Number(a.slot_free)) || bySpeed(a, b),
    speed: bySpeed,
    files: (a, b) => (b.files.length - a.files.length) || bySpeed(a, b),
  };
  list.sort(sorters[f.sort] || sorters.best);
  for (const group of list) group.files.sort((a, b) => a.name.localeCompare(b.name));
  return list;
}

function renderSlskResultsHead() {
  const head = $("#slsk-results-head");
  const entry = slsk.results.get(slsk.searchId);
  if (!head || !entry || !entry.summary) return;
  const s = entry.summary;
  head.textContent = `${s.files} FILES FROM ${s.replies} USERS${s.active ? " · SEARCHING…" : s.stopped ? " · STOPPED" : ""}`;
}

function slskDownloadState(username, path) {
  return slsk.wanted.get(`${username}\n${path}`);
}

function slskGetButton(username, file) {
  const known = slskDownloadState(username, file.name);
  if (known && known !== "failed" && known !== "cancelled") {
    const done = SLSK_TERMINAL.has(known);
    return h("button", {
      type: "button", class: done ? "icon-btn done" : "icon-btn",
      title: SLSK_STATES[known] || known, "aria-label": SLSK_STATES[known] || known,
      onclick: () => slskShowPane("transfers"),
    }, icon(done ? "check" : "clock"));
  }
  const btn = h("button", {
    type: "button", class: "icon-btn primary", title: "Download to the library",
    "aria-label": `Download ${baseName(file.name)}`,
  }, icon("download"));
  btn.addEventListener("click", async () => {
    btn.disabled = true;
    try {
      await api("/api/soulseek/downloads", {
        method: "POST", body: { username, files: [{ path: file.name, size: file.size }] },
      });
      toast(`Queued “${baseName(file.name)}” from ${username}`);
      slsk.wanted.set(`${username}\n${file.name}`, "queued");
      btn.replaceWith(slskGetButton(username, file));
    } catch (err) {
      toast(err.message, true);
      btn.disabled = false;
    }
  });
  return btn;
}

async function slskGetFolder(username, folder, btn) {
  btn.disabled = true;
  const label = btn.textContent;
  btn.textContent = "[ FETCHING… ]";
  try {
    const res = await api("/api/soulseek/downloads", { method: "POST", body: { username, folder } });
    toast(res.queued
      ? `Queued ${res.queued} file(s) from ${username}; progress in TRANSFERS`
      : "Everything in this folder is already queued");
    await slskLoadDownloads();
    btn.textContent = "[ QUEUED ]";
  } catch (err) {
    toast(err.message, true);
    btn.disabled = false;
    btn.textContent = label;
  }
}

function slskUserButtons(username) {
  return [
    h("button", { type: "button", class: "btn", onclick: () => slskOpenBrowse(username) }, "[ BROWSE ]"),
    h("button", { type: "button", class: "btn", onclick: () => slskOpenChat(username) }, "[ MSG ]"),
  ];
}

function slskFileRow(username, file) {
  const a = file.attrs || {};
  return h("div", { class: "slsk-file" },
    h("div", { class: "title keep-case", title: file.name }, baseName(file.name)),
    h("div", { class: "slsk-q" }, slskQuality(file)),
    h("div", { class: "slsk-size" }, formatBytes(file.size)),
    h("div", { class: "duration" }, a.duration_secs ? formatDuration(a.duration_secs) : ""),
    slskGetButton(username, file));
}

function slskGroupEl(group) {
  const folderName = baseName(group.folder) || group.folder || "(root)";
  const getFolder = h("button", { type: "button", class: "btn btn-primary",
    title: "Queue every audio file of this folder (and its cover art)" }, "[ GET FOLDER ]");
  getFolder.addEventListener("click", () => slskGetFolder(group.username, group.folder, getFolder));
  return h("div", { class: "slsk-group" },
    h("div", { class: "slsk-group-head" },
      h("div", { class: "title-cell" },
        h("div", { class: "title", title: group.folder }, folderName),
        h("div", { class: "subtitle", title: group.folder }, group.folder)),
      h("div", { class: "slsk-peer" },
        h("span", { class: "keep-case slsk-name" }, group.username),
        h("span", { class: group.slot_free ? "slsk-free" : "slsk-busy" },
          group.slot_free ? "FREE SLOT" : `QUEUE ${group.queue_length}`),
        group.avg_speed ? h("span", { class: "dim" }, formatSpeed(group.avg_speed)) : null),
      h("div", { class: "row-actions slsk-actions" }, getFolder, slskUserButtons(group.username))),
    group.files.map((file) => slskFileRow(group.username, file)));
}

function renderSlskResults() {
  slsk.dirty = false;
  const container = $("#slsk-results");
  const entry = slsk.results.get(slsk.searchId);
  if (slsk.searchId == null || !entry) {
    container.replaceChildren(h("div", { class: "empty" },
      h("p", { class: "empty-title" }, ">> READY TO DIG."),
      h("p", {}, "RESULTS ARRIVE OVER A MINUTE OR SO AS USERS ANSWER. GROUPED BY USER AND FOLDER: "
        + "[GET FOLDER] QUEUES A WHOLE RELEASE WITH ITS COVER ART.")));
    return;
  }
  const groups = slskGroups(entry.replies);
  const summary = entry.summary || {};
  const stop = summary.active ? h("button", {
    type: "button", class: "btn",
    onclick: async () => {
      try { await api(`/api/soulseek/searches/${slsk.searchId}/stop`, { method: "POST" }); slskSchedule(0); }
      catch (err) { toast(err.message, true); }
    },
  }, "[ STOP ]") : null;
  const children = [h("div", { class: "view-header slsk-results-bar" },
    h("p", { class: "dim", id: "slsk-results-head" }), stop)];
  if (groups.length === 0) {
    children.push(h("div", { class: "empty" },
      h("p", { class: "empty-title" }, summary.active ? ">> WAITING FOR ANSWERS." : ">> 0 MATCHING FILES."),
      h("p", {}, entry.replies.length
        ? "NOTHING PASSES THE FILTERS. LOOSEN THEM ABOVE."
        : summary.active ? "PEERS ARE SEARCHING THEIR SHARES." : "TRY OTHER WORDS, OR ADD IT TO YOUR WISHLIST.")));
  } else {
    children.push(...groups.slice(0, slsk.shown).map(slskGroupEl));
    if (groups.length > slsk.shown) {
      children.push(h("div", { class: "list-foot" }, h("button", {
        type: "button", class: "btn",
        onclick: () => { slsk.shown += SLSK_PAGE; renderSlskResults(); },
      }, `[ SHOW MORE: ${groups.length - slsk.shown} FOLDERS LEFT ]`)));
    } else {
      children.push(listFoot(`${groups.length} FOLDERS`));
    }
  }
  container.replaceChildren(...children);
  renderSlskResultsHead();
}

// ---------- transfers ----------

async function slskLoadDownloads() {
  try {
    slsk.downloads = await api("/api/soulseek/downloads");
  } catch {
    return;
  }
  const before = slsk.wanted;
  slsk.wanted = new Map();
  // Oldest first so a newer attempt's state wins.
  for (const d of [...slsk.downloads].reverse()) slsk.wanted.set(`${d.username}\n${d.path}`, d.state);
  let changed = before.size !== slsk.wanted.size;
  if (!changed) for (const [k, v] of slsk.wanted) if (before.get(k) !== v) { changed = true; break; }
  if (changed) slsk.dirty = true;
  if (changed && state.view === "library") loadLibrary();
}

async function slskLoadUploads() {
  try {
    slsk.uploads = await api("/api/soulseek/uploads");
  } catch {
    // Transient.
  }
}

function slskProgress(bytes, size) {
  const width = NARROW.matches ? 14 : 26;
  const fraction = size ? Math.min(1, Math.max(0, bytes / size)) : 0;
  const filled = Math.round(fraction * width);
  const percent = Math.round(fraction * 100);
  return h("div", { class: "progress", role: "progressbar", "aria-valuemin": "0", "aria-valuemax": "100",
    "aria-valuenow": String(percent) },
  `[${"#".repeat(filled)}${".".repeat(width - filled)}] ${String(percent).padStart(3)}%`);
}

function slskDownloadLabel(d) {
  if (d.state === "remote" && d.place != null) return `Queue #${d.place}`;
  return SLSK_STATES[d.state] || d.state;
}

function slskDownloadRow(d) {
  const actions = [];
  if (!SLSK_TERMINAL.has(d.state)) {
    actions.push(h("button", {
      type: "button", class: "icon-btn", title: "Cancel", "aria-label": `Cancel ${baseName(d.path)}`,
      onclick: async () => {
        try { await api(`/api/soulseek/downloads/${d.id}/cancel`, { method: "POST" }); slskSchedule(0); }
        catch (err) { toast(err.message, true); }
      },
    }, icon("close")));
  }
  if (d.state === "failed" || d.state === "cancelled") {
    actions.push(h("button", {
      type: "button", class: "icon-btn", title: "Try again", "aria-label": `Retry ${baseName(d.path)}`,
      onclick: async () => {
        try { await api(`/api/soulseek/downloads/${d.id}/retry`, { method: "POST" }); slskSchedule(0); }
        catch (err) { toast(err.message, true); }
      },
    }, "RETRY"));
  }
  // A quality upgrade is filed under the track it upgrades, as its lossless copy.
  const output = d.upgrade_for ? "lossless" : "original";
  if (d.track_key && d.destination) {
    actions.push(saveLink(libraryFileUrl(d.track_key.provider, d.track_key.id, output), d.destination, baseName(d.path)));
  }
  const playable = d.track_key && d.destination && (d.state === "completed" || d.state === "skipped");
  const facts = [];
  if (d.state === "transferring") {
    facts.push(h("span", {}, `${formatBytes(d.bytes)} / ${formatBytes(d.size)}`,
      d.speed_bytes_per_sec ? ` · ${formatSpeed(d.speed_bytes_per_sec)}` : ""));
  } else if (!d.cover) {
    facts.push(h("span", {}, formatBytes(d.size)));
  }
  if (d.quality) {
    facts.push(h("span", {}, "File ", h("b", {}, d.quality.summary),
      d.quality.lossless ? h("span", { class: "lossless" }, " · lossless") : null));
    if (d.quality.spectrum) facts.push(fact("Spectrum", d.quality.spectrum.summary));
  }
  if (d.artwork) facts.push(fact("Artwork", { file: "embedded", folder: "from folder", none: "none" }[d.artwork] || d.artwork));
  if (d.attempts) facts.push(fact("Attempts", String(d.attempts)));
  return h("div", { class: "slsk-dl" },
    playable
      ? playableArt({
        key: `slsk:${d.track_key.id}`, title: baseName(d.path), artists: [d.username], artwork: null, duration: null,
        request: { source: "library", provider: d.track_key.provider, id: d.track_key.id, output },
      }, null)
      : h("span", { class: "art slsk-art", "aria-hidden": "true" }, d.cover ? "IMG" : extOf(d.path).toUpperCase()),
    h("div", { class: "job-main" },
      h("div", { class: "job-top" },
        h("div", { class: "title keep-case", title: d.path }, baseName(d.path)),
        h("span", { class: `state ${d.state === "transferring" ? "downloading" : d.state}` }, slskDownloadLabel(d))),
      d.state === "transferring" ? slskProgress(d.bytes, d.size) : null,
      d.upgrade_for ? h("div", { class: "note" }, `QUALITY UPGRADE for “${d.upgrade_for}”`) : null,
      h("div", { class: "facts" }, facts.filter(Boolean)),
      d.destination ? h("div", { class: "path", title: "Location in library" }, d.destination) : null,
      d.inferred && d.inferred.length
        ? h("div", { class: "warn-line" }, `Guessed from the folder and file name — please check: ${d.inferred.map((f) => f.replace("_", " ")).join(", ")}`)
        : null,
      d.note ? h("div", { class: "note" }, d.note) : null,
      d.error ? h("div", { class: "error-line" }, d.error + (d.retryable ? " (temporary — retry later)" : "")) : null),
    h("div", { class: "row-actions" }, actions));
}

function renderSlskTransfers() {
  if (slsk.pane !== "transfers") return;
  const container = $("#slsk-downloads");
  if (!slsk.downloads.length) {
    container.replaceChildren(h("div", { class: "empty" }, h("p", { class: "empty-title" }, ">> NOTHING QUEUED."),
      h("p", {}, "QUEUE FILES OR WHOLE FOLDERS FROM SEARCH RESULTS OR A USER'S SHARES. "
        + "USERS UPLOAD WHEN THEY HAVE A FREE SLOT; WAITING IN A QUEUE IS NORMAL.")));
  } else {
    // Group by user and folder, newest group first.
    const groups = new Map();
    for (const d of slsk.downloads) {
      const key = `${d.username}\n${dirName(d.path)}`;
      if (!groups.has(key)) groups.set(key, []);
      groups.get(key).push(d);
    }
    container.replaceChildren(...Array.from(groups.values()).map((items) => {
      const first = items[0];
      const tracks = items.filter((d) => !d.cover);
      const done = tracks.filter((d) => d.state === "completed" || d.state === "skipped").length;
      const failed = items.filter((d) => d.state === "failed");
      const retryFailed = failed.length ? h("button", {
        type: "button", class: "btn",
        onclick: async () => {
          for (const d of failed) {
            try { await api(`/api/soulseek/downloads/${d.id}/retry`, { method: "POST" }); } catch { /* shown per row */ }
          }
          slskSchedule(0);
        },
      }, `[ RETRY ${failed.length} FAILED ]`) : null;
      items.sort((a, b) => Number(b.cover) - Number(a.cover) || a.path.localeCompare(b.path));
      return h("div", { class: "slsk-group" },
        h("div", { class: "slsk-group-head" },
          h("div", { class: "title-cell" },
            h("div", { class: "title", title: dirName(first.path) }, baseName(dirName(first.path)) || "(root)"),
            h("div", { class: "subtitle" }, `${done}/${tracks.length} DONE`)),
          h("div", { class: "slsk-peer" }, h("span", { class: "keep-case slsk-name" }, first.username)),
          h("div", { class: "row-actions slsk-actions" }, retryFailed, slskUserButtons(first.username))),
        items.map(slskDownloadRow));
    }));
  }

  const uploads = $("#slsk-uploads");
  if (!slsk.uploads.length) {
    uploads.replaceChildren(h("p", { class: "dim setting-note" }, slsk.status && slsk.status.share_library
      ? "Nobody is downloading from you right now."
      : "Sharing is off, so nobody can download from you. Turn it on under SETUP."));
    return;
  }
  uploads.replaceChildren(...slsk.uploads.map((u) => h("div", { class: "slsk-dl" },
    h("span", { class: "art slsk-art", "aria-hidden": "true" }, "UP"),
    h("div", { class: "job-main" },
      h("div", { class: "job-top" },
        h("div", { class: "title keep-case", title: u.path }, baseName(u.path)),
        h("span", { class: `state ${u.state === "transferring" ? "downloading" : u.state}` }, SLSK_UP_STATES[u.state] || u.state)),
      h("div", { class: "subtitle" }, `to ${u.username}`),
      u.state === "transferring" ? slskProgress(u.bytes, u.size) : null,
      u.state === "transferring" && u.speed_bytes_per_sec ? h("div", { class: "facts" }, formatSpeed(u.speed_bytes_per_sec)) : null,
      u.error ? h("div", { class: "error-line" }, u.error) : null),
    h("div", { class: "row-actions" }, SLSK_UP_STATES[u.state] && !["completed", "failed", "cancelled"].includes(u.state)
      ? h("button", {
        type: "button", class: "icon-btn", title: "Cancel upload", "aria-label": `Cancel upload of ${baseName(u.path)}`,
        onclick: async () => {
          try { await api(`/api/soulseek/uploads/${u.id}/cancel`, { method: "POST" }); slskSchedule(0); }
          catch (err) { toast(err.message, true); }
        },
      }, icon("close"))
      : null))));
}

// ---------- browse ----------

function slskOpenBrowse(username) {
  $("#slsk-browse-user").value = username;
  slskShowPane("browse");
  slskBrowse(username, false);
}

async function slskBrowse(username, refresh) {
  slsk.browseUser = username;
  slsk.browse = null;
  slsk.browseOpen = null;
  slsk.browseFiles = null;
  slsk.userInfo = null;
  slsk.browseFilter = "";
  renderSlskBrowse();
  try {
    slsk.browse = await api("/api/soulseek/browse", { method: "POST", body: { username, refresh } });
  } catch (err) {
    toast(err.message, true);
  }
  renderSlskBrowse();
  api(`/api/soulseek/users/${encodeURIComponent(username)}`)
    .then((info) => {
      if (slsk.browseUser !== username) return;
      slsk.userInfo = info;
      renderSlskUserCard();
    })
    .catch((err) => {
      if (slsk.browseUser !== username) return;
      slsk.userInfo = { username, info_error: err.message };
      renderSlskUserCard();
    });
  slskSchedule(1500);
}

async function slskRefreshBrowse() {
  if (!slsk.browseUser || !slsk.browse || slsk.browse.status !== "loading") return;
  try {
    slsk.browse = await api(`/api/soulseek/browse/${encodeURIComponent(slsk.browseUser)}`);
    if (slsk.browse.status !== "loading") renderSlskBrowse();
  } catch {
    // Transient.
  }
}

function renderSlskUserCard() {
  const card = $("#slsk-usercard");
  if (!card) return;
  const info = slsk.userInfo;
  if (!info) {
    card.replaceChildren(h("p", { class: "dim" }, "Fetching user info…"));
    return;
  }
  const pic = h("img", { class: "slsk-pic", alt: "" });
  if (typeof info.picture === "string" && info.picture.startsWith("data:image/")) pic.src = info.picture;
  else pic.hidden = true;
  const facts = [
    fact("Status", info.status ? info.status.toUpperCase() : null),
    fact("Speed", info.avg_speed ? formatSpeed(info.avg_speed) : null),
    fact("Shares", info.shared_files != null ? `${info.shared_files} files / ${info.shared_folders} folders` : null),
    fact("Slots", info.slots_free == null ? null : info.slots_free ? "free" : "busy"),
    fact("Queue", info.queue_size != null ? String(info.queue_size) : null),
    fact("Uploads", info.uploads != null ? String(info.uploads) : null),
  ].filter(Boolean);
  card.replaceChildren(pic, h("div", { class: "slsk-userinfo" },
    h("div", { class: "facts" }, facts),
    info.description ? h("p", { class: "slsk-desc keep-case" }, info.description) : null,
    info.info_error ? h("p", { class: "note" }, info.info_error) : null));
}

async function slskOpenDir(path) {
  if (slsk.browseOpen === path) {
    slsk.browseOpen = null;
    slsk.browseFiles = null;
    renderSlskBrowse();
    return;
  }
  slsk.browseOpen = path;
  slsk.browseFiles = null;
  renderSlskBrowse();
  try {
    const params = new URLSearchParams({ dir: path });
    slsk.browseFiles = await api(`/api/soulseek/browse/${encodeURIComponent(slsk.browseUser)}/files?${params}`);
  } catch (err) {
    toast(err.message, true);
    slsk.browseFiles = [];
  }
  renderSlskBrowse();
}

function renderSlskBrowse() {
  const container = $("#slsk-browse");
  const user = slsk.browseUser;
  if (!user) {
    container.replaceChildren(h("div", { class: "empty" }, h("p", { class: "empty-title" }, ">> WHOSE CRATES?"),
      h("p", {}, "ENTER A USERNAME, OR PRESS [BROWSE] NEXT TO ANY SEARCH RESULT. "
        + "PEOPLE WHO HAVE ONE GREAT RECORD OFTEN HAVE MORE.")));
    return;
  }
  const b = slsk.browse;
  const searchForm = h("form", { class: "search-bar slsk-usersearch", autocomplete: "off" },
    h("label", { class: "prompt", for: "slsk-usersearch" }, "SEARCH THEIR FILES>"),
    h("input", { id: "slsk-usersearch", maxlength: "200", placeholder: "title, artist, folder…", "aria-label": `Search ${user}'s files` }),
    h("button", { type: "submit", class: "btn btn-primary" }, "[ RUN ]"));
  searchForm.addEventListener("submit", (event) => {
    event.preventDefault();
    const query = searchForm.querySelector("input").value.trim();
    if (query) slskSearch(query, user);
  });
  const head = h("div", { class: "slsk-user-head" },
    h("div", { class: "view-header" },
      h("p", { class: "section-title keep-case" }, user),
      h("div", { class: "row-actions" },
        h("button", { type: "button", class: "btn", onclick: () => slskOpenChat(user) }, "[ MSG ]"),
        h("button", { type: "button", class: "btn", onclick: () => slskBrowse(user, true) }, "[ REFRESH ]"))),
    h("div", { class: "slsk-usercard", id: "slsk-usercard" }),
    searchForm);
  const children = [head];
  if (!b || b.status === "loading") {
    children.push(h("div", { class: "empty" }, h("p", { class: "empty-title" }, ">> REQUESTING SHARE LIST."),
      h("p", {}, "BIG COLLECTIONS TAKE A WHILE TO ARRIVE.")));
  } else if (b.status === "failed") {
    children.push(h("p", { class: "error-line" }, b.error || "The share list could not be fetched."));
  } else {
    const filterInput = h("input", {
      type: "search", class: "filter", placeholder: "folder names", "aria-label": "Filter folders",
    });
    filterInput.value = slsk.browseFilter;
    filterInput.addEventListener("input", () => {
      slsk.browseFilter = filterInput.value;
      renderSlskDirs();
    });
    children.push(
      h("div", { class: "view-header" },
        h("p", { class: "dim" }, `${b.total_files} FILES IN ${b.dirs.length} FOLDERS`),
        h("label", { class: "filter-wrap" }, h("span", { class: "prompt" }, "FILTER>"), filterInput)),
      h("div", { id: "slsk-dirs", class: "slsk-dirs" }));
  }
  container.replaceChildren(...children);
  renderSlskUserCard();
  if (b && b.status === "ready") renderSlskDirs();
}

function renderSlskDirs() {
  const container = $("#slsk-dirs");
  const b = slsk.browse;
  if (!container || !b) return;
  const filter = slsk.browseFilter.trim();
  const dirs = b.dirs.filter((d) => slskTextMatches(d.path, filter));
  const limit = 500;
  const rows = dirs.slice(0, limit).map((d) => {
    const depth = Math.max(0, d.path.split("\\").length - 1);
    const open = slsk.browseOpen === d.path;
    const get = h("button", { type: "button", class: "btn btn-primary" }, "[ GET ]");
    get.addEventListener("click", () => slskGetFolder(slsk.browseUser, d.path, get));
    const row = h("div", { class: open ? "slsk-dir open" : "slsk-dir" },
      h("button", {
        type: "button", class: "slsk-dir-name keep-case", title: d.path,
        style: null, onclick: () => slskOpenDir(d.path),
      }, `${open ? "[-]" : "[+]"} ${baseName(d.path) || d.path}`),
      h("span", { class: "dim slsk-dir-count" }, `${d.files} FILES · ${formatBytes(d.bytes)}`),
      d.files ? get : h("span"));
    row.style.setProperty("--depth", String(Math.min(depth, 12)));
    const out = [row];
    if (open) {
      out.push(h("div", { class: "slsk-dir-files" },
        slsk.browseFiles == null
          ? h("div", { class: "skeleton" })
          : slsk.browseFiles.length
            ? slsk.browseFiles.map((f) => {
              const file = { ...f, name: `${d.path}\\${f.name}` };
              return SLSK_AUDIO.has(extOf(f.name))
                ? slskFileRow(slsk.browseUser, file)
                : h("div", { class: "slsk-file other" },
                  h("div", { class: "title keep-case dim" }, f.name), h("div"), h("div", { class: "slsk-size" }, formatBytes(f.size)), h("div"), h("span"));
            })
            : h("p", { class: "dim" }, "Empty folder.")));
    }
    return out;
  });
  container.replaceChildren(...rows.flat(),
    listFoot(dirs.length > limit ? `SHOWING ${limit} OF ${dirs.length} FOLDERS; FILTER TO NARROW DOWN` : `${dirs.length} FOLDERS`));
}

// ---------- messages ----------

async function slskLoadChats() {
  try {
    slsk.convos = await api("/api/soulseek/chats");
  } catch {
    return;
  }
  if (slsk.chatUser) {
    try {
      slsk.chat = await api(`/api/soulseek/chats/${encodeURIComponent(slsk.chatUser)}`);
    } catch {
      // Transient.
    }
  }
  renderSlskChats();
}

function slskOpenChat(username) {
  slsk.chatUser = username;
  slsk.chat = { username, messages: [] };
  slskShowPane("messages");
  slskLoadChats().then(() => {
    const input = $("#slsk-chat-text");
    if (input) input.focus();
  });
}

function formatClock(ms) {
  const d = new Date(ms);
  const two = (n) => String(n).padStart(2, "0");
  const today = new Date().toDateString() === d.toDateString();
  return `${today ? "" : `${d.getFullYear()}-${two(d.getMonth() + 1)}-${two(d.getDate())} `}${two(d.getHours())}:${two(d.getMinutes())}`;
}

function renderSlskChats() {
  if (slsk.pane !== "messages") return;
  const list = $("#slsk-convos");
  if (!slsk.convos.length && !slsk.chatUser) {
    list.replaceChildren(h("p", { class: "dim setting-note" },
      "No conversations yet. Private messages to you appear here; a polite note before queueing a whole discography goes a long way."));
  } else {
    list.replaceChildren(...slsk.convos.map((c) => h("button", {
      type: "button", class: c.username === slsk.chatUser ? "slsk-convo-item active" : "slsk-convo-item",
      onclick: () => slskOpenChat(c.username),
    },
    h("span", { class: "keep-case slsk-name" }, c.username),
    c.unread ? h("span", { class: "badge" }, String(c.unread)) : null,
    c.last ? h("span", { class: "subtitle" }, c.last.text) : null)));
  }

  const view = $("#slsk-convo");
  if (!slsk.chatUser) {
    view.replaceChildren();
    delete view.dataset.user;
    return;
  }
  const messages = (slsk.chat && slsk.chat.username === slsk.chatUser) ? slsk.chat.messages : [];
  const log = h("div", { class: "slsk-log", id: "slsk-log" }, messages.length
    ? messages.map((m) => h("div", { class: m.from_me ? "slsk-msg me" : "slsk-msg" },
      h("span", { class: "dim" }, `${formatClock(m.at_ms)} `),
      h("span", { class: "slsk-msg-from keep-case" }, m.from_me ? "you" : slsk.chatUser),
      h("span", { class: "dim" }, "> "),
      h("span", { class: "slsk-msg-text" }, m.text)))
    : h("p", { class: "dim" }, "No messages yet."));
  // Polling refreshes only the log, so typing in the open conversation is
  // never disturbed.
  const existing = $("#slsk-log");
  if (existing && view.dataset.user === slsk.chatUser) {
    const atBottom = existing.scrollTop + existing.clientHeight >= existing.scrollHeight - 8;
    existing.replaceWith(log);
    log.scrollTop = atBottom ? log.scrollHeight : existing.scrollTop;
    return;
  }
  view.dataset.user = slsk.chatUser;
  const input = h("input", { id: "slsk-chat-text", class: "filter", maxlength: "2000",
    placeholder: "message", "aria-label": `Message to ${slsk.chatUser}` });
  const form = h("form", { class: "search-bar", autocomplete: "off" },
    h("label", { class: "prompt", for: "slsk-chat-text" }, "SAY>"), input,
    h("button", { type: "submit", class: "btn btn-primary" }, "[ SEND ]"));
  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    const text = input.value.trim();
    if (!text) return;
    try {
      slsk.chat = await api(`/api/soulseek/chats/${encodeURIComponent(slsk.chatUser)}`, { method: "POST", body: { text } });
      input.value = "";
      await slskLoadChats();
      const fresh = $("#slsk-log");
      if (fresh) fresh.scrollTop = fresh.scrollHeight;
    } catch (err) {
      toast(err.message, true);
    }
  });
  view.replaceChildren(
    h("div", { class: "view-header" },
      h("p", { class: "section-title keep-case" }, slsk.chatUser),
      h("div", { class: "row-actions" },
        h("button", { type: "button", class: "btn", onclick: () => slskOpenBrowse(slsk.chatUser) }, "[ BROWSE ]"),
        h("button", {
          type: "button", class: "btn",
          onclick: async () => {
            try { await api(`/api/soulseek/chats/${encodeURIComponent(slsk.chatUser)}`, { method: "DELETE" }); }
            catch (err) { toast(err.message, true); }
            slsk.chatUser = null;
            slsk.chat = null;
            slskLoadChats();
          },
        }, "[ CLOSE ]"))),
    log, form);
  log.scrollTop = log.scrollHeight;
}

// ---------- setup ----------

function renderSlskSetup() {
  const s = slsk.status;
  if (!s || slsk.pane !== "setup") return;
  $("#slsk-account").textContent = `Signed in as ${s.username}. ${s.remembered
    ? "Inkult reconnects automatically after a restart (the password is stored on this server)."
    : "You will need to sign in again after Inkult restarts."}`;
  for (const chip of document.querySelectorAll("#slsk-share .chip")) {
    const active = (chip.dataset.share === "on") === Boolean(s.share_library);
    chip.classList.toggle("active", active);
    chip.setAttribute("aria-checked", String(active));
  }
  $("#slsk-share-detail").textContent = s.share_library
    ? `Sharing ${s.shared_files} file(s) in ${s.shared_folders} folder(s) from your Inkult library, read-only. `
      + "Other users see them under an \"inkult\" folder; your server's paths stay private."
    : "Only tracks in your own Inkult library would be shared. Many users only serve people who share, "
      + "so turning this on helps your own downloads.";
  $("#slsk-port").textContent = s.listen_port
    ? `Listening for peers on port ${s.listen_port}. Forward this TCP port on your router (and map it in Docker) `
      + "so users behind firewalls can reach you; without it, Inkult still works through outgoing connections."
    : "No listening port is free (see INKULT_SOULSEEK_PORTS). Inkult works through outgoing connections only, "
      + "so users who are also behind a firewall cannot be reached.";
  const wishes = s.wishlist || [];
  $("#slsk-wishlist").replaceChildren(...(wishes.length
    ? wishes.map((term) => h("div", { class: "slsk-wish" },
      h("span", { class: "keep-case" }, term),
      h("button", {
        type: "button", class: "icon-btn", title: "Remove", "aria-label": `Remove ${term} from the wishlist`,
        onclick: async () => {
          try {
            await api("/api/soulseek/wishlist/remove", { method: "POST", body: { term } });
            await slskLoadStatus();
          } catch (err) { toast(err.message, true); }
        },
      }, "X")))
    : [h("p", { class: "dim setting-note" }, "Nothing on the wishlist.")]));
  const notices = s.notices || [];
  $("#slsk-notices-wrap").hidden = notices.length === 0;
  $("#slsk-notices").replaceChildren(...notices.map((n) => h("p", { class: "note" }, n)));
}

function setupSoulseek() {
  $("#slsk-signin-form").addEventListener("submit", slskSignIn);
  $("#slsk-signout").addEventListener("click", slskSignOut);
  for (const chip of document.querySelectorAll("#slsk-tabs .chip")) {
    chip.addEventListener("click", () => { slskShowPane(chip.dataset.pane); slskSchedule(0); });
  }
  $("#slsk-search-form").addEventListener("submit", (event) => {
    event.preventDefault();
    const query = $("#slsk-query").value.trim();
    if (query) slskSearch(query);
  });
  $("#slsk-filter").addEventListener("input", (event) => {
    slsk.filter.text = event.target.value.trim();
    slsk.shown = SLSK_PAGE;
    renderSlskResults();
  });
  chipGroup("#slsk-format .chip", (chip) => { slsk.filter.format = chip.dataset.format; renderSlskResults(); });
  chipGroup("#slsk-bitrate .chip", (chip) => { slsk.filter.kbps = Number(chip.dataset.kbps); renderSlskResults(); });
  chipGroup("#slsk-sort .chip", (chip) => { slsk.filter.sort = chip.dataset.sort; renderSlskResults(); });
  $("#slsk-free-only").addEventListener("change", (event) => {
    slsk.filter.freeOnly = event.target.checked;
    renderSlskResults();
  });
  $("#slsk-clear-downloads").addEventListener("click", async () => {
    try { await api("/api/soulseek/downloads/finished", { method: "DELETE" }); slskSchedule(0); }
    catch (err) { toast(err.message, true); }
  });
  $("#slsk-clear-uploads").addEventListener("click", async () => {
    try { await api("/api/soulseek/uploads/finished", { method: "DELETE" }); slskSchedule(0); }
    catch (err) { toast(err.message, true); }
  });
  $("#slsk-browse-form").addEventListener("submit", (event) => {
    event.preventDefault();
    const user = $("#slsk-browse-user").value.trim();
    if (user) slskBrowse(user, false);
  });
  $("#slsk-chat-new").addEventListener("submit", (event) => {
    event.preventDefault();
    const user = $("#slsk-chat-user").value.trim();
    if (user) {
      $("#slsk-chat-user").value = "";
      slskOpenChat(user);
    }
  });
  for (const chip of document.querySelectorAll("#slsk-share .chip")) {
    chip.addEventListener("click", async () => {
      const share = chip.dataset.share === "on";
      if (slsk.status && Boolean(slsk.status.share_library) === share) return;
      try {
        slsk.status = await api("/api/soulseek/settings", { method: "POST", body: { share_library: share } });
        toast(share ? "Sharing your library on Soulseek" : "Stopped sharing");
        renderSlskStatus();
      } catch (err) {
        toast(err.message, true);
      }
    });
  }
  $("#slsk-wish-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    const term = $("#slsk-wish").value.trim();
    if (!term) return;
    try {
      await api("/api/soulseek/wishlist", { method: "POST", body: { term } });
      $("#slsk-wish").value = "";
      toast(`“${term}” added to the wishlist; searching now`);
      await slskLoadStatus();
    } catch (err) {
      toast(err.message, true);
    }
  });
}

// ---------- logo fire ----------

// The INKULT lettering burns: a small "doom fire" simulation runs through
// the logo's own text, so the logo keeps exactly its size. Heat rises from a
// hidden row under the letters; letter cells are fuel (they cool slowly and
// never go dark), empty cells only show flame tongues while hot. A low frame
// rate keeps it a flicker rather than an animation, and cheap.

const FIRE_FPS = 8;
/** Characters for flame tongues outside the letters, by heat level 1–4. */
const FIRE_TONGUES = [" ", ".", ":", "^", "*"];

/** Fuel mask (1 = letter cell) for ASCII art, centred in a w × height grid. */
function maskOf(lines, w, height) {
  const mask = new Uint8Array(w * height);
  const left = Math.max(0, Math.floor((w - Math.max(...lines.map((l) => l.length))) / 2));
  const top = Math.max(0, Math.floor((height - lines.length) / 2));
  lines.slice(0, height).forEach((line, y) => {
    for (let x = 0; x < line.length && left + x < w; x++) {
      mask[(top + y) * w + left + x] = line[x] === " " ? 0 : 1;
    }
  });
  return mask;
}

function fireFor(pre) {
  const lines = pre.textContent.replace(/^\n/, "").replace(/\n$/, "").split("\n");
  const w = Math.max(...lines.map((l) => l.length));
  const height = lines.length;
  const mask = maskOf(lines, w, height);
  pre.classList.add("fire");
  return {
    pre, w, height, mask,
    /** The logo's own grid and lettering, restored after the easter egg. */
    home: { w, height, mask: mask.slice() },
    // One extra row below the logo feeds the flames.
    heat: new Float32Array(w * (height + 1)),
    /** Per cell: 0, or 1 + index into BURN_CHARS while a letter burns away. */
    charred: new Uint8Array(w * height),
    /** Running easter egg (see startFireEgg), or null. */
    egg: null,
    blaze: false,
    wind: 0,
  };
}

function stepFire(f) {
  const { w, height, mask, heat } = f;
  if (Math.random() < 0.1) f.wind = [-1, 0, 0, 1][Math.floor(Math.random() * 4)];
  // Fed from under the letters only, so the gaps between them stay
  // readable and flames rise around the lettering rather than over it.
  const base = (height - 1) * w;
  for (let x = 0; x < w; x++) {
    heat[height * w + x] = mask[base + x] || f.blaze ? 0.7 + Math.random() * 0.3 : Math.random() * 0.45;
  }
  // While blazing, flames cool slower and lick over the whole logo. While
  // the easter-egg banner stands they cool faster: its letters are only one
  // column apart, and tongues in those gaps would make it unreadable.
  const cooling = f.blaze ? 0.5 : f.egg ? 1.8 : 1;
  // Top-down, reading the row below from the previous frame: flames rise
  // one row per frame.
  for (let y = 0; y < height; y++) {
    for (let x = 0; x < w; x++) {
      const drift = Math.floor(Math.random() * 3) - 1 + f.wind;
      const sx = Math.min(w - 1, Math.max(0, x + drift));
      const below = heat[(y + 1) * w + sx];
      let v;
      if (mask[y * w + x]) {
        // Burning fuel: carries heat up, never goes dark, and flares up
        // in random spots so the whole word flickers.
        v = Math.max(below - 0.03 - Math.random() * 0.08, 0.3 + Math.random() * 0.25);
        if (Math.random() < 0.04) v = 0.8 + Math.random() * 0.2;
      } else {
        v = below - (0.1 + Math.random() * 0.14) * cooling;
      }
      heat[y * w + x] = Math.max(0, v);
    }
  }
}

function renderFire(f) {
  const { w, height, mask, heat, charred } = f;
  const rows = [];
  for (let y = 0; y < height; y++) {
    if (y) rows.push("\n");
    let run = "";
    let runClass = null;
    const flush = () => {
      if (!run) return;
      rows.push(runClass ? h("span", { class: runClass }, run) : run);
      run = "";
    };
    for (let x = 0; x < w; x++) {
      const v = heat[y * w + x];
      const level = Math.min(4, Math.floor(v * 5));
      let ch;
      let cls;
      if (charred[y * w + x]) {
        const stage = charred[y * w + x] - 1;
        ch = BURN_CHARS[stage];
        cls = `f${5 - stage}`;
      } else if (mask[y * w + x]) {
        ch = level >= 4 ? "@" : "#";
        cls = `f${level + 1}`;
      } else if (level >= 1) {
        ch = FIRE_TONGUES[level];
        cls = `f${level}`;
      } else {
        ch = " ";
        cls = null;
      }
      if (cls !== runClass) {
        flush();
        runClass = cls;
      }
      run += ch;
    }
    flush();
  }
  f.pre.replaceChildren(...rows);
}

// Easter egg: clicking the logo burns INKULT down, "4 THA KULTUR" rises
// from the flames, stands a moment, burns down the same way, and INKULT
// rises again. Each swap is a transition: the old letters burn away from
// the bottom row up (flaring white-hot, then crumbling to ash) while the
// feed row blazes across the whole width, then the new letters ignite out
// of the flames, also bottom-up.
//
// The banner is drawn in the logo's own lettering, which needs almost twice
// the logo's columns. So it gets its own, finer grid in the same box: more
// columns and rows of smaller characters (--logo-scale). The grids swap at
// the height of the blaze, when the box is nothing but flames.

const EGG_TEXT = [
  "###   ###     #########  ###   ###   #######      ###   ###  ###   ###  ###        #########  ###   ###  ########",
  "###   ###        ###     ###   ###  ###   ###     ###  ###   ###   ###  ###           ###     ###   ###  ###   ###",
  "###   ###        ###     ###   ###  ###   ###     ### ###    ###   ###  ###           ###     ###   ###  ###   ###",
  "#########        ###     #########  #########     ######     ###   ###  ###           ###     ###   ###  ########",
  "      ###        ###     ###   ###  ###   ###     ### ###    ###   ###  ###           ###     ###   ###  ### ###",
  "      ###        ###     ###   ###  ###   ###     ###  ###   ###   ###  ###           ###     ###   ###  ###  ###",
  "      ###        ###     ###   ###  ###   ###     ###   ###   #######   #########     ###      #######   ###   ###",
];
/** Columns of flame either side of the banner. */
const EGG_MARGIN = 2;
/** Frame rate while the egg runs; the idle flicker is too coarse for it. */
const EGG_FPS = 18;
/** A burning letter cell, from flare-up to ash. */
const BURN_CHARS = ["@", "%", "*", "."];
/** How long one letter cell takes to burn away. */
const BURN_MS = 500;
/** Burning and igniting sweep from the bottom row to the top over this long… */
const SWEEP_MS = 800;
/** …with this much random spread per cell, so the edge is ragged. */
const SPREAD_MS = 400;
/** The grids swap once the old letters are ash… */
const SWAP_AT_MS = SWEEP_MS + SPREAD_MS + BURN_MS + 100;
/** …and the new letters start igniting shortly after. */
const IGNITE_AT_MS = SWAP_AT_MS + 200;
/** How long a freshly ignited cell stays white-hot. */
const IGNITE_MS = 500;
/** How long "4 THA KULTUR" stands before burning in turn. */
const EGG_HOLD_MS = 2400;
/** Static display time when the user prefers reduced motion. */
const EGG_STILL_MS = 3000;

/** The banner's grid: as wide as it needs, as many rows as fill the box. */
function bannerGrid(f) {
  const home = f.home;
  const w = Math.max(home.w, Math.max(...EGG_TEXT.map((l) => l.length)) + 2 * EGG_MARGIN);
  const height = Math.max(EGG_TEXT.length, Math.round((home.height * w) / home.w));
  return { w, height, mask: maskOf(EGG_TEXT, w, height) };
}

/**
 * Switches the fire to `grid`, carrying the flames over, and scales the
 * characters so the box keeps the home grid's size.
 */
function regrid(f, grid) {
  const { w, height } = grid;
  const heat = new Float32Array(w * (height + 1));
  for (let y = 0; y <= height; y++) {
    const sy = Math.round((y * f.height) / height);
    for (let x = 0; x < w; x++) heat[y * w + x] = f.heat[sy * f.w + Math.floor((x * f.w) / w)];
  }
  Object.assign(f, { w, height, heat, mask: grid.mask.slice(), charred: new Uint8Array(w * height) });

  const style = f.pre.style;
  if (grid === f.home) {
    style.removeProperty("--logo-scale");
    style.removeProperty("line-height");
    return;
  }
  // Read the home metrics before scaling anything.
  if (!f.lineRatio) {
    const css = getComputedStyle(f.pre);
    f.lineRatio = parseFloat(css.lineHeight) / parseFloat(css.fontSize) || 1.05;
  }
  const scale = f.home.w / w;
  style.setProperty("--logo-scale", String(scale));
  style.lineHeight = String((f.lineRatio * f.home.height) / (height * scale));
}

/** When each lit cell of `mask` hits the sweep, bottom row first. */
function sweepTimes(grid, offset) {
  const { w, height, mask } = grid;
  const at = new Float32Array(mask.length).fill(Infinity);
  for (let i = 0; i < mask.length; i++) {
    if (!mask[i]) continue;
    const fromBottom = (height - 1 - Math.floor(i / w)) / Math.max(1, height - 1);
    at[i] = offset + fromBottom * SWEEP_MS + Math.random() * SPREAD_MS;
  }
  return at;
}

function fireTransition(from, to, start) {
  return {
    from, to, start,
    burnAt: sweepTimes(from, 0),
    igniteAt: sweepTimes(to, IGNITE_AT_MS),
    swapped: false,
    end: IGNITE_AT_MS + SWEEP_MS + SPREAD_MS + IGNITE_MS,
  };
}

/** Sets the fuel mask, ash and forced heat for `elapsed` ms into a transition. */
function applyTransition(f, tr, elapsed) {
  const e = elapsed - tr.start;
  f.blaze = e < tr.end;
  if (e >= SWAP_AT_MS && !tr.swapped) {
    regrid(f, tr.to);
    tr.swapped = true;
  }
  const { mask, charred, heat } = f;
  for (let i = 0; i < mask.length; i++) {
    charred[i] = 0;
    if (tr.swapped) {
      // The new letters ignite out of the flames.
      const lit = e - tr.igniteAt[i];
      mask[i] = lit >= 0 ? 1 : 0;
      if (lit >= 0 && lit < IGNITE_MS) heat[i] = Math.max(heat[i], 1 - (lit / IGNITE_MS) * 0.35);
      continue;
    }
    // The old letters burn away; a burning cell is still fuel, so it feeds
    // the flames above it.
    const burnt = (e - tr.burnAt[i]) / BURN_MS;
    if (!tr.from.mask[i] || burnt >= 1) {
      mask[i] = 0;
    } else if (burnt >= 0) {
      charred[i] = 1 + Math.floor(burnt * BURN_CHARS.length);
      heat[i] = 1;
    }
  }
}

function startFireEgg(f) {
  if (f.egg) return;
  const banner = bannerGrid(f);
  if (window.matchMedia("(prefers-reduced-motion: reduce)").matches) {
    // No animation: show the banner still, then the logo again.
    const show = (grid) => {
      regrid(f, grid);
      for (let i = 0; i < 12; i++) stepFire(f);
      renderFire(f);
    };
    f.egg = { still: true };
    show(banner);
    setTimeout(() => {
      show(f.home);
      f.egg = null;
    }, EGG_STILL_MS);
    return;
  }
  const away = fireTransition(f.home, banner, 0);
  const back = fireTransition(banner, f.home, away.end + EGG_HOLD_MS);
  f.egg = { start: performance.now(), steps: [away, back], end: back.start + back.end };
}

function stepFireEgg(f, now) {
  const elapsed = now - f.egg.start;
  if (elapsed >= f.egg.end) {
    if (f.w !== f.home.w || f.height !== f.home.height) regrid(f, f.home);
    f.mask.set(f.home.mask);
    f.charred.fill(0);
    f.blaze = false;
    f.egg = null;
    return;
  }
  const current = f.egg.steps.findLast((tr) => elapsed >= tr.start);
  applyTransition(f, current, elapsed);
}

function setupLogoFire() {
  const fires = Array.from(document.querySelectorAll("pre.logo")).map(fireFor);
  const still = window.matchMedia("(prefers-reduced-motion: reduce)");
  // Warm up so the first frame already burns.
  for (const f of fires) {
    for (let i = 0; i < 12; i++) stepFire(f);
    renderFire(f);
    f.pre.addEventListener("click", () => startFireEgg(f));
  }
  const tick = () => {
    if (!document.hidden && !still.matches) {
      const now = performance.now();
      for (const f of fires) {
        // Only the logo on screen (login gate or app) is animated.
        if (f.pre.offsetParent === null) continue;
        stepFire(f);
        if (f.egg) stepFireEgg(f, now);
        renderFire(f);
      }
    }
    const egg = fires.some((f) => f.egg && !f.egg.still);
    setTimeout(tick, 1000 / (egg ? EGG_FPS : FIRE_FPS));
  };
  setTimeout(tick, 1000 / FIRE_FPS);
}

// ---------- clock ----------

/** How often the crew's online count is refreshed. */
const ONLINE_POLL_MS = 15000;

async function loadOnline() {
  if (!state.user) return;
  try {
    const { online } = await api("/api/online");
    $("#online-count").textContent = String(online);
  } catch {
    // Transient; the next refresh retries.
  }
}

function tickClock() {
  const now = new Date();
  const two = (n) => String(n).padStart(2, "0");
  $("#clock-time").textContent = `${two(now.getHours())}:${two(now.getMinutes())}:${two(now.getSeconds())}`;
  $("#clock-date").textContent = `${two(now.getMonth() + 1)}-${two(now.getDate())}-${two(now.getFullYear() % 100)}`;
}

// ---------- messages (chat rooms) ----------

const ROOM_HELP = "Commands: /me <action>, /topic [text], /join <room>, /part";
/** Lines kept in the open room's log, like the server's history. */
const ROOM_MAX_LINES = 500;

const rooms = {
  /** Every room on the server, as summaries. */
  list: [],
  /** JSON of the last rendered list, to skip needless re-renders. */
  listKey: "",
  /** Open room id, or null. */
  current: null,
  /** Last view of the open room: { room, users }. */
  view: null,
  messages: [],
  lastSeq: 0,
  timer: null,
  ticking: false,
};

function updateRoomsBadge(total) {
  const badge = $("#rooms-badge");
  badge.textContent = String(total);
  badge.hidden = !total;
}

function roomsSchedule(delay) {
  clearTimeout(rooms.timer);
  if (!state.user) return;
  rooms.timer = setTimeout(roomsTick, delay);
}

async function roomsTick() {
  if (rooms.ticking) return;
  rooms.ticking = true;
  try {
    if (state.view === "messages") {
      // The open room first: viewing it marks it read before the list
      // (and the badge) is refreshed.
      await roomsLoadCurrent();
      await roomsLoadList();
    } else {
      const { unread } = await api("/api/rooms/unread");
      updateRoomsBadge(unread);
    }
  } catch {
    // Transient; the next tick retries.
  } finally {
    rooms.ticking = false;
  }
  roomsSchedule(state.view === "messages" ? 2000 : 15000);
}

function roomsEnter() {
  rooms.listKey = "";
  renderRoomList();
  renderRoom([], true);
  roomsSchedule(0);
}

async function roomsLoadList() {
  rooms.list = await api("/api/rooms");
  updateRoomsBadge(rooms.list.reduce((n, r) => n + r.unread, 0));
  renderRoomList();
}

async function roomsLoadCurrent() {
  const id = rooms.current;
  if (id == null) return;
  const after = rooms.lastSeq ? `?after=${rooms.lastSeq}` : "";
  let view;
  try {
    view = await api(`/api/rooms/${id}${after}`);
  } catch (err) {
    // Deleted by its owner, or this account was removed from it.
    if (rooms.current === id && (err.status === 404 || err.status === 403)) {
      toast(err.message, true);
      roomsClose();
    }
    return;
  }
  if (rooms.current === id) roomsApply(view, Boolean(after));
}

function roomsApply(view, incremental) {
  const fresh = incremental ? view.messages.filter((m) => m.seq > rooms.lastSeq) : view.messages;
  rooms.messages = (incremental ? rooms.messages.concat(fresh) : fresh).slice(-ROOM_MAX_LINES);
  if (rooms.messages.length) rooms.lastSeq = rooms.messages[rooms.messages.length - 1].seq;
  rooms.view = view;
  const i = rooms.list.findIndex((r) => r.id === view.room.id);
  if (i >= 0) rooms.list[i] = view.room;
  else rooms.list.push(view.room);
  renderRoom(fresh, !incremental);
}

function roomsClose() {
  rooms.current = null;
  rooms.view = null;
  rooms.messages = [];
  rooms.lastSeq = 0;
  renderRoom([], true);
  rooms.listKey = "";
  renderRoomList();
}

async function roomsOpen(id) {
  // Picked from the rooms panel on a phone: show the chat.
  closePanel({ restoreFocus: false });
  if (rooms.current === id) {
    $("#room-input")?.focus();
    return;
  }
  const summary = rooms.list.find((r) => r.id === id);
  rooms.current = id;
  rooms.view = null;
  rooms.messages = [];
  rooms.lastSeq = 0;
  renderRoom([], true);
  try {
    const view = summary && summary.joined
      ? await api(`/api/rooms/${id}`)
      : await api(`/api/rooms/${id}/join`, { method: "POST" });
    if (rooms.current !== id) return;
    roomsApply(view, false);
    rooms.listKey = "";
    await roomsLoadList();
    $("#room-input")?.focus();
  } catch (err) {
    toast(err.message, true);
    if (rooms.current === id) roomsClose();
  }
}

async function roomsCreate(event) {
  event.preventDefault();
  const input = $("#room-new-name");
  const secret = $("#room-new-password");
  const name = input.value.trim();
  if (!name) return;
  // A password makes the room private; left empty, the room is public.
  const body = secret.value ? { name, password: secret.value } : { name };
  try {
    const view = await api("/api/rooms", { method: "POST", body });
    input.value = "";
    secret.value = "";
    await roomsShow(view);
  } catch (err) {
    toast(err.message, true);
  }
}

/** Show a room just created or joined. */
async function roomsShow(view) {
  rooms.current = view.room.id;
  rooms.lastSeq = 0;
  roomsApply(view, false);
  rooms.listKey = "";
  await roomsLoadList();
  $("#room-input")?.focus();
}

/**
 * Ask for a private room's name and password and join it; a refusal
 * reopens the dialog with the server's reason.
 */
async function roomsJoinPrivate() {
  let name = $("#room-new-name").value.trim().replace(/^#/, "");
  let error = "";
  for (;;) {
    const values = await askDialog({
      title: "JOIN A PRIVATE ROOM",
      text: "Ask a member for the room's name and password. You only need them once.",
      fields: [
        { name: "name", label: "ROOM #", value: name, maxlength: 33 },
        { name: "password", label: "PASSWORD", type: "password", value: "" },
      ],
      submit: "[ JOIN ]",
      error,
      validate: (v) => {
        if (!v.name.trim()) return "Enter the room's name.";
        if (!v.password) return "Enter the password.";
        return "";
      },
    });
    if (!values) return;
    name = values.name.trim();
    try {
      const view = await api("/api/rooms/join-private", {
        method: "POST", body: { name, password: values.password },
      });
      $("#room-new-name").value = "";
      closePanel({ restoreFocus: false });
      await roomsShow(view);
      return;
    } catch (err) {
      error = err.message;
    }
  }
}

async function roomsLeave() {
  const id = rooms.current;
  if (id == null) return;
  const room = rooms.view && rooms.view.room;
  if (room && room.private) {
    const alone = rooms.view.users.length <= 1;
    const ok = await askDialog({
      title: `LEAVE #${room.name.toUpperCase()}?`,
      text: alone
        ? "You are the last member: leaving deletes this private room and its whole history."
        : "It is private: coming back takes its name and password again.",
      submit: "[ LEAVE ]",
      danger: alone,
    });
    if (!ok || rooms.current !== id) return;
  }
  try {
    await api(`/api/rooms/${id}/leave`, { method: "POST" });
  } catch (err) {
    toast(err.message, true);
    return;
  }
  roomsClose();
  roomsLoadList().catch(() => {});
}

async function roomsDelete() {
  const room = rooms.view && rooms.view.room;
  if (!room) return;
  const ok = await askDialog({
    title: `DELETE #${room.name.toUpperCase()}?`,
    text: room.private
      ? "The room and its whole history are deleted for everyone, and its password stops working. This cannot be undone."
      : "The room and its whole history are deleted for everyone. This cannot be undone.",
    submit: "[ DELETE ]",
    danger: true,
  });
  if (!ok) return;
  try {
    await api(`/api/rooms/${room.id}`, { method: "DELETE" });
  } catch (err) {
    toast(err.message, true);
    return;
  }
  toast(`#${room.name} deleted`);
  roomsClose();
  roomsLoadList().catch(() => {});
}

async function roomsSetTopic(topic) {
  const room = rooms.view && rooms.view.room;
  if (!room) return;
  try {
    await api(`/api/rooms/${room.id}`, { method: "PATCH", body: { topic } });
    await roomsLoadCurrent();
  } catch (err) {
    toast(err.message, true);
  }
}

async function roomsAskTopic() {
  const room = rooms.view && rooms.view.room;
  if (!room) return;
  const values = await askDialog({
    title: `TOPIC FOR #${room.name.toUpperCase()}`,
    text: "Leave empty to clear it.",
    fields: [{ name: "topic", label: "TOPIC", value: room.topic || "", maxlength: 200 }],
    submit: "[ SET ]",
  });
  if (values) await roomsSetTopic(values.topic);
}

/** IRC-style commands typed into the message line. */
async function roomsCommand(text) {
  const [command, ...rest] = text.slice(1).split(/\s+/);
  const arg = text.slice(1 + command.length).trim();
  switch (command.toLowerCase()) {
    case "me":
      if (arg) return false;
      toast("Usage: /me <action>", true);
      return true;
    case "topic":
      if (!rooms.view || !rooms.view.room.can_moderate) toast("Only the room's owner can change the topic.", true);
      else await roomsSetTopic(arg);
      return true;
    case "join": {
      const name = (rest[0] || "").replace(/^#/, "").toLowerCase();
      const room = rooms.list.find((r) => r.name === name);
      if (room) await roomsOpen(room.id);
      else if (!name) toast("Usage: /join <room>", true);
      else {
        // Not listed: it may be private, which takes its password.
        $("#room-new-name").value = name;
        await roomsJoinPrivate();
      }
      return true;
    }
    case "part":
    case "leave":
      await roomsLeave();
      return true;
    default:
      toast(`Unknown command /${command}. ${ROOM_HELP}`, true);
      return true;
  }
}

async function roomsSend(event) {
  event.preventDefault();
  const input = $("#room-input");
  const text = input.value.trim();
  if (!text || rooms.current == null) return;
  if (text.startsWith("/") && await roomsCommand(text)) {
    input.value = "";
    return;
  }
  try {
    await api(`/api/rooms/${rooms.current}/messages`, { method: "POST", body: { text } });
    input.value = "";
    await roomsLoadCurrent();
    const log = $("#room-log");
    if (log) log.scrollTop = log.scrollHeight;
  } catch (err) {
    toast(err.message, true);
  }
}

function roomButton(room) {
  const active = room.id === rooms.current;
  return h("button", {
    type: "button", class: active ? "irc-room active" : "irc-room",
    "aria-current": active ? "true" : null,
    title: room.topic || null,
    onclick: () => roomsOpen(room.id),
  },
  h("span", { class: "irc-room-name keep-case" }, `#${room.name}`,
    room.private ? h("span", { class: "irc-lock dim", title: "Private room" }, "[P]") : null),
  room.unread ? h("span", { class: "badge" }, String(room.unread)) : null,
  h("span", { class: "irc-room-count dim" }, room.joined ? `${room.online}/${room.members}` : `${room.members}`),
  !room.joined && room.topic ? h("span", { class: "irc-room-topic subtitle" }, room.topic) : null);
}

function renderRoomList() {
  if (state.view !== "messages") return;
  const key = JSON.stringify([rooms.current, rooms.list]);
  if (key === rooms.listKey) return;
  rooms.listKey = key;
  const joined = rooms.list.filter((r) => r.joined);
  const others = rooms.list.filter((r) => !r.joined);
  $("#rooms-joined").replaceChildren(...(joined.length
    ? joined.map(roomButton)
    : [h("p", { class: "dim setting-note" }, "None yet. Pick one below.")]));
  $("#rooms-all").replaceChildren(...(others.length
    ? others.map(roomButton)
    : [h("p", { class: "dim setting-note" }, rooms.list.length ? "You are in every room." : "No rooms yet. Start one:")]));
}

/**
 * A track shared in a room. Members can play it straight away and add it
 * to their library, which takes no download: the file is already on disk.
 */
function sharedTrackCard(m, roomId) {
  const t = m.track;
  const status = m.track_status;
  const item = {
    key: `src:${t.source_url}`,
    title: t.title,
    artists: t.artists,
    artwork: t.artwork_url,
    duration: null,
    request: status === "in_library"
      ? { source: "library", provider: t.key.provider, id: t.key.id, output: t.output }
      : { source: "shared", room: roomId, seq: m.seq },
  };
  let action;
  if (status === "available") {
    action = h("button", { type: "button", class: "btn", "aria-label": `Add ${t.title} to my library` }, "[ + MY LIBRARY ]");
    action.addEventListener("click", async () => {
      action.disabled = true;
      try {
        await api(`/api/rooms/${roomId}/tracks/${m.seq}/claim`, { method: "POST" });
        toast(`“${t.title}” added to your library`);
        card.replaceWith(sharedTrackCard({ ...m, track_status: "in_library" }, roomId));
      } catch (err) {
        toast(err.message, true);
        action.disabled = false;
      }
    });
  } else {
    action = h("span", { class: "dim" }, status === "in_library" ? "IN YOUR LIBRARY" : "NO LONGER AVAILABLE");
  }
  const q = t.quality;
  const card = h("div", { class: "irc-track" },
    status === "gone" ? art(t.artwork_url) : playableArt(item, t.artwork_url),
    h("div", { class: "title-cell" },
      h("div", { class: "title", title: t.title }, t.title),
      h("div", { class: "subtitle" }, [t.artists.join(", "), t.album].filter(Boolean).join(" · ")),
      h("div", { class: "irc-track-facts dim" }, providerTag(t.key.provider), ` ${OUTPUT_SHORT[t.output] || t.output} · ${q.summary}`,
        q.lossless ? h("span", { class: "lossless" }, " · lossless") : null,
        q.converted_from ? ` · converted from ${q.converted_from}` : null)),
    h("div", { class: "row-actions" }, action));
  return card;
}

function roomLine(m, roomName, roomId) {
  const time = h("span", { class: "irc-time" }, `[${formatClock(m.at_ms)}] `);
  const me = state.user ? state.user.username : "";
  let cls = "irc-line";
  let body;
  switch (m.kind) {
    case "text":
      body = [h("span", { class: "irc-nick" }, `<${m.from}>`), " ", h("span", { class: "irc-text" }, m.text)];
      break;
    case "action":
      cls += " irc-action";
      body = [`* ${m.from} `, h("span", { class: "irc-text" }, m.text)];
      break;
    case "joined":
      cls += " irc-event";
      body = [`-!- ${m.from} has joined #${roomName}`];
      break;
    case "left":
      cls += " irc-event";
      body = [`-!- ${m.from} has left #${roomName}`];
      break;
    case "track":
      cls += " irc-track-line";
      body = [h("span", { class: "irc-nick" }, `<${m.from}>`), " shared a track:", sharedTrackCard(m, roomId)];
      break;
    case "topic":
      cls += " irc-event";
      body = [m.text ? `-!- ${m.from} changed the topic to: ${m.text}` : `-!- ${m.from} cleared the topic`];
      break;
    default:
      return null;
  }
  if (m.mine) cls += " me";
  else if (me && (m.kind === "text" || m.kind === "action")
    && new RegExp(`(^|[^\\w.-])${me.replace(/[.]/g, "\\.")}($|[^\\w-])`, "i").test(m.text)) {
    cls += " mention";
  }
  return h("div", { class: cls }, time, ...body);
}

function renderRoomUsers() {
  const panel = $("#room-users");
  if (!rooms.view) {
    panel.hidden = true;
    panel.replaceChildren();
    return;
  }
  const users = rooms.view.users;
  const online = users.filter((u) => u.online).length;
  panel.hidden = false;
  panel.replaceChildren(
    h("p", { class: "irc-heading" }, `USERS ${online}/${users.length}:`),
    h("ul", { class: "irc-user-list" }, users.map((u) => h("li", {},
      h("button", {
        type: "button", class: u.online ? "irc-user keep-case" : "irc-user keep-case dim",
        title: `${u.owner ? "Room owner, " : ""}${u.online ? "here now" : "away"}; click to address`,
        onclick: () => {
          const input = $("#room-input");
          if (!input) return;
          input.value = `${u.name}: ${input.value}`;
          input.focus();
        },
      }, `${u.owner ? "@" : " "}${u.name}`)))));
}

function renderRoomHead(room) {
  return h("div", { class: "irc-head", id: "room-head" },
    h("div", { class: "irc-head-text" },
      h("p", { class: "irc-room-title keep-case" }, `#${room.name}`,
        room.private ? h("span", { class: "irc-lock dim" }, "[PRIVATE]") : null),
      h("p", { class: "irc-topic keep-case" }, room.topic || h("span", { class: "dim" }, "no topic")),
      h("p", { class: "dim irc-owner" }, "OWNER: ", h("span", { class: "keep-case" }, room.owner))),
    h("div", { class: "row-actions" },
      room.can_moderate ? h("button", { type: "button", class: "btn", onclick: roomsAskTopic }, "[ TOPIC ]") : null,
      h("button", { type: "button", class: "btn", onclick: roomsLeave }, "[ LEAVE ]"),
      room.can_moderate ? h("button", { type: "button", class: "btn danger", onclick: roomsDelete }, "[ DELETE ]") : null));
}

/**
 * Show the open room. Polling only appends `fresh` lines and swaps the
 * header, so the message being typed is never disturbed.
 */
function renderRoom(fresh, full) {
  if (state.view !== "messages") return;
  const main = $("#room-main");
  if (rooms.current == null) {
    delete main.dataset.room;
    main.replaceChildren(h("div", { class: "irc-empty" },
      h("p", {}, "NO ROOM OPEN."),
      h("p", { class: "dim setting-note" }, "Join a room from the room list or start your own. Rooms are shared by everyone on this Inkult server; private ones are joined with their name and password."),
      h("button", { type: "button", class: "btn btn-primary compact-only", onclick: () => openPanel("rooms") }, "[ BROWSE ROOMS ]"),
      h("p", { class: "dim setting-note" }, ROOM_HELP)));
    renderRoomUsers();
    return;
  }
  if (!rooms.view) {
    main.replaceChildren(h("p", { class: "dim" }, "JOINING..."));
    renderRoomUsers();
    return;
  }
  const room = rooms.view.room;
  const lines = (list) => list.map((m) => roomLine(m, room.name, room.id)).filter(Boolean);
  const existingLog = $("#room-log");
  if (!full && existingLog && main.dataset.room === String(room.id)) {
    $("#room-head").replaceWith(renderRoomHead(room));
    if (fresh.length) {
      const atBottom = existingLog.scrollTop + existingLog.clientHeight >= existingLog.scrollHeight - 8;
      existingLog.append(...lines(fresh));
      while (existingLog.childElementCount > ROOM_MAX_LINES) existingLog.firstElementChild.remove();
      if (atBottom) existingLog.scrollTop = existingLog.scrollHeight;
    }
    renderRoomUsers();
    return;
  }
  main.dataset.room = String(room.id);
  const log = h("div", { class: "irc-log", id: "room-log", role: "log", "aria-live": "polite",
    "aria-label": `Messages in #${room.name}` }, lines(rooms.messages));
  const input = h("input", { id: "room-input", class: "filter", maxlength: "1000",
    placeholder: "message or /command", "aria-label": `Message to #${room.name}` });
  const form = h("form", { class: "search-bar irc-say", autocomplete: "off", onsubmit: roomsSend },
    h("label", { class: "prompt", for: "room-input" }, "SAY>"), input,
    h("button", { type: "submit", class: "btn btn-primary" }, "[ SEND ]"));
  main.replaceChildren(renderRoomHead(room), log, form);
  log.scrollTop = log.scrollHeight;
  renderRoomUsers();
}

// ---------- dialog ----------

/**
 * Modal form. `fields`: [{ name, label, type, autocomplete, value, maxlength,
 * inputmode, placeholder }]. Resolves to the entered values, or null if
 * cancelled. `validate(values)` may return an error message to keep the
 * dialog open; `error` is shown on opening. `extra` is a node shown between
 * the text and the fields; `cancel: null` hides the cancel button.
 */
function askDialog({ title, text = "", extra = null, fields = [], submit = "[ OK ]", cancel = "[ CANCEL ]",
  danger = false, validate, error: initialError = "" }) {
  const dialog = $("#dialog");
  $("#dialog-title").textContent = title;
  $("#dialog-text").textContent = text;
  $("#dialog-text").hidden = !text;
  $("#dialog-extra").replaceChildren(...(extra ? [extra] : []));
  $("#dialog-cancel").hidden = cancel == null;
  if (cancel != null) $("#dialog-cancel").textContent = cancel;
  const error = $("#dialog-error");
  error.textContent = initialError;
  error.hidden = !initialError;
  const inputs = fields.map((field, i) => {
    const id = `dialog-field-${i}`;
    return {
      field,
      label: h("label", { for: id, class: "prompt" }, `${field.label}>`),
      input: h("input", {
        id, class: "field", type: field.type || "text",
        autocomplete: field.autocomplete || "off", maxlength: String(field.maxlength || 128),
        value: field.value, inputmode: field.inputmode, placeholder: field.placeholder,
        autocapitalize: "none", spellcheck: "false",
      }),
    };
  });
  $("#dialog-fields").replaceChildren(...inputs.flatMap(({ label, input }) => [label, input]));
  $("#dialog-fields").hidden = inputs.length === 0;
  const submitBtn = $("#dialog-submit");
  submitBtn.textContent = submit;
  submitBtn.classList.toggle("danger", danger);

  return new Promise((resolve) => {
    const form = $("#dialog-form");
    const finish = (value) => {
      form.removeEventListener("submit", onSubmit);
      $("#dialog-cancel").removeEventListener("click", onCancel);
      dialog.removeEventListener("cancel", onCancel);
      if (dialog.open) dialog.close();
      resolve(value);
    };
    const onSubmit = (event) => {
      event.preventDefault();
      const values = Object.fromEntries(inputs.map(({ field, input }) => [field.name, input.value]));
      const problem = validate && validate(values);
      if (problem) {
        error.textContent = problem;
        error.hidden = false;
        return;
      }
      finish(values);
    };
    const onCancel = (event) => {
      event.preventDefault();
      finish(null);
    };
    form.addEventListener("submit", onSubmit);
    $("#dialog-cancel").addEventListener("click", onCancel);
    dialog.addEventListener("cancel", onCancel);
    dialog.showModal();
    (inputs[0] ? inputs[0].input : submitBtn).focus();
  });
}

/**
 * Keep asking with `options` until `run(values)` succeeds; its error is shown
 * in the dialog so the user can correct the entry. Null if cancelled.
 */
async function askUntil(options, run) {
  let error = options.error || "";
  for (;;) {
    const values = await askDialog({ ...options, error });
    if (!values) return null;
    try {
      return await run(values);
    } catch (err) {
      error = err.message;
    }
  }
}

const MIN_PASSWORD = 12;

function newPasswordProblem(password, confirm) {
  if (password.length < MIN_PASSWORD) return `Password must be at least ${MIN_PASSWORD} characters.`;
  if (password !== confirm) return "The passwords do not match.";
  return null;
}

// ---------- login gate ----------

/**
 * `challenge`: the server's token for the code step after a right password.
 * `password`: what was typed at the password step, kept in memory only
 * until the session starts, so a required password change or 2FA setup
 * right after login doesn't ask for it again.
 */
const gate = { mode: "login", challenge: null, password: null };

const GATE_MODES = {
  login: { intro: "AUTHORIZED CREW ONLY. IDENTIFY YOURSELF.", submit: "[ LOGIN ]", focus: "#gate-user" },
  setup: {
    intro: "NEW SYSTEM. NO CREW YET. APPOINT THE CAPTAIN WITH THE SETUP CODE PRINTED IN THE SERVER LOG.",
    submit: "[ APPOINT CAPTAIN ]", focus: "#gate-code",
  },
  otp: {
    intro: "ENTER THE 6-DIGIT CODE FROM YOUR AUTHENTICATOR APP. LOST YOUR PHONE? ENTER ONE OF YOUR RECOVERY CODES.",
    submit: "[ VERIFY ]", focus: "#gate-otp",
  },
};

/**
 * Show the login screen ("login"), first-captain setup ("setup") or the
 * authenticator-code step of a login ("otp").
 */
function showGate(mode, message = "") {
  gate.mode = mode;
  if (mode !== "otp") {
    gate.challenge = null;
    gate.password = null;
  }
  const spec = GATE_MODES[mode];
  $("#app").hidden = true;
  $("#gate").hidden = false;
  for (const el of document.querySelectorAll("[data-gate]")) el.hidden = !el.dataset.gate.split(" ").includes(mode);
  $("#gate-intro").textContent = spec.intro;
  $("#gate-pass").autocomplete = mode === "setup" ? "new-password" : "current-password";
  $("#gate-submit").textContent = spec.submit;
  showGateError(message);
  $(spec.focus).focus();
}

function showGateError(message) {
  const el = $("#gate-error");
  el.textContent = message;
  el.hidden = !message;
}

/** Logged in: enter the app (or start clean if a session already ran). */
async function gateLoggedIn(user) {
  for (const id of ["#gate-pass", "#gate-confirm", "#gate-code", "#gate-otp"]) $(id).value = "";
  const password = gate.password;
  gate.challenge = null;
  gate.password = null;
  // A page that already ran a session holds the previous user's state;
  // start clean instead of reusing it.
  if (state.started) location.reload();
  else await startSession(user, password);
}

/** Run a gate request with the submit button showing progress. */
async function gateBusy(run) {
  const button = $("#gate-submit");
  button.disabled = true;
  button.textContent = "[ DIALING... ]";
  showGateError("");
  try {
    await run();
  } finally {
    button.disabled = false;
    button.textContent = GATE_MODES[gate.mode].submit;
  }
}

async function submitGateCode() {
  const code = $("#gate-otp").value.trim();
  if (!code) {
    showGateError("Enter the code from your authenticator app.");
    return;
  }
  await gateBusy(async () => {
    let user;
    try {
      user = await api("/api/auth/login/2fa", { method: "POST", body: { challenge: gate.challenge, code } });
    } catch (err) {
      // 410: the challenge is gone (too slow, too many wrong codes); 423:
      // the account just got locked. Either way the password comes first.
      if (err.status === 410 || err.status === 423) {
        showGate("login", err.message);
        return;
      }
      showGateError(err.message);
      $("#gate-otp").select();
      return;
    }
    await gateLoggedIn(user);
  });
}

async function submitGate(event) {
  event.preventDefault();
  if (gate.mode === "otp") {
    await submitGateCode();
    return;
  }
  const setup = gate.mode === "setup";
  const username = $("#gate-user").value.trim();
  const password = $("#gate-pass").value;
  if (!username || !password) {
    showGateError("Enter your handle and password.");
    return;
  }
  if (setup) {
    const problem = newPasswordProblem(password, $("#gate-confirm").value);
    if (problem) { showGateError(problem); return; }
  }
  await gateBusy(async () => {
    let res;
    gate.password = password;
    try {
      res = setup
        ? await api("/api/auth/setup", { method: "POST", body: { code: $("#gate-code").value, username, password } })
        : await api("/api/auth/login", { method: "POST", body: { username, password } });
    } catch (err) {
      showGateError(err.message);
      $("#gate-pass").select();
      return;
    }
    if (res.second_factor_required) {
      $("#gate-pass").value = "";
      $("#gate-otp").value = "";
      showGate("otp");
      gate.challenge = res.challenge;
      gate.password = password;
      return;
    }
    await gateLoggedIn(res);
  });
}

/** The server no longer accepts our session: stop and ask to log in. */
function sessionLost() {
  if (!state.user) return;
  state.user = null;
  clearTimeout(state.pollTimer);
  clearTimeout(slsk.timer);
  clearTimeout(rooms.timer);
  if (player.audio) player.audio.pause();
  closeFormatMenu();
  if ($("#dialog").open) $("#dialog").close();
  showGate("login", "SESSION ENDED. PLEASE LOG IN AGAIN.");
}

// ---------- two-factor login ----------

const PASSWORD_FIELD = { name: "password", label: "PASSWORD", type: "password", autocomplete: "current-password" };
/** A current authenticator code, or a recovery code instead. */
const SECOND_FACTOR_FIELD = {
  name: "code", label: "CODE", autocomplete: "one-time-code", maxlength: 24,
  placeholder: "6 digits or a recovery code",
};
const reauthProblem = (v) => (!v.password ? "Enter your password."
  : !v.code.trim() ? "Enter a code from your authenticator app, or a recovery code." : null);

function downloadText(filename, text) {
  const url = URL.createObjectURL(new Blob([text], { type: "text/plain;charset=utf-8" }));
  const link = h("a", { href: url, download: filename, hidden: true });
  document.body.append(link);
  link.click();
  link.remove();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

/** Show freshly made recovery codes; the server never shows them again. */
async function showRecoveryCodes(codes, title, username) {
  const file = `Inkult recovery codes for ${username} (${location.host})\n` +
    `Created ${new Date().toISOString().slice(0, 10)}. Each code works once.\n\n${codes.join("\n")}\n`;
  await askDialog({
    title,
    text: "Lost your phone? Each recovery code logs you in once instead of an authenticator code. " +
      "Keep them somewhere safe, away from this device: a password manager, or on paper. " +
      "They are shown only now.",
    extra: h("div", {},
      h("div", { class: "twofa-codes", "aria-label": "Recovery codes" }, codes.map((code) => h("span", {}, code))),
      h("button", { type: "button", class: "btn",
        onclick: () => downloadText(`inkult-recovery-codes-${username}.txt`, file) }, "[ SAVE AS .TXT ]")),
    submit: "[ I SAVED THEM ]",
    cancel: null,
  });
}

/** The QR code and manual key for adding Inkult to an authenticator app. */
function twoFactorSetupView(setup) {
  return h("div", {},
    h("img", {
      class: "twofa-qr", alt: `QR code adding ${setup.issuer} (${setup.account}) to an authenticator app`,
      src: `data:image/svg+xml;charset=utf-8,${encodeURIComponent(setup.qr_svg)}`,
    }),
    h("p", {}, "Can't scan it? Add an account by hand with this key " +
      `(time-based, ${setup.digits} digits, every ${setup.period} s):`),
    h("code", { class: "twofa-key" }, setup.secret),
    h("p", {}, "On this phone? ",
      h("a", { href: setup.uri, class: "keep-case" }, "Open it in your authenticator app"), "."));
}

/**
 * Add an authenticator: show the QR code, then confirm with a code and the
 * password. Resolves to the account (the server issues a new session and
 * logs off other browsers), or null if cancelled. `forced`: the server
 * requires 2FA, so cancelling means logging off.
 */
async function enrollTwoFactor({ forced = false, password = null } = {}) {
  let setup;
  try {
    setup = await api("/api/auth/2fa/setup", { method: "POST" });
  } catch (err) {
    toast(err.message, true);
    return null;
  }
  let problem = "";
  for (;;) {
    const values = await askDialog({
      error: problem,
      title: forced ? "TWO-FACTOR LOGIN REQUIRED" : "SET UP TWO-FACTOR LOGIN",
      text: (forced ? "Your account needs a code from an authenticator app at every login. " : "") +
        "1. Scan the QR code with your authenticator app (Google Authenticator, Microsoft Authenticator, " +
        `Aegis, 2FAS, 1Password…). 2. Enter the 6-digit code it shows${password ? "" : ", and your password"}. ` +
        "Other browsers logged in as you will be logged off.",
      extra: twoFactorSetupView(setup),
      fields: [
        { name: "code", label: "CODE", inputmode: "numeric", autocomplete: "one-time-code", maxlength: 7, placeholder: "6 digits" },
        password ? null : PASSWORD_FIELD,
      ].filter(Boolean),
      submit: "[ TURN ON ]",
      cancel: forced ? "[ LOG OFF ]" : "[ CANCEL ]",
      validate: (v) => (!/^\d{6}$/.test(v.code.replace(/[\s-]/g, "")) ? "Enter the 6-digit code your app shows."
        : !password && !v.password ? "Enter your password." : null),
    });
    if (!values) return null;
    try {
      const body = { code: values.code, password: password ?? values.password };
      const account = await api("/api/auth/2fa/enable", { method: "POST", body });
      await showRecoveryCodes(account.recovery_codes, "TWO-FACTOR LOGIN IS ON", account.username);
      return account;
    } catch (err) {
      problem = err.message;
      // A wrong remembered password (should not happen): ask for it.
      if (err.status === 401 && /password/.test(err.message)) password = null;
      // The QR code was left open too long: show a new one.
      if (err.status === 409) {
        try {
          setup = await api("/api/auth/2fa/setup", { method: "POST" });
          problem = "That QR code expired. Remove it from your app if you added it, then scan this new one.";
        } catch (again) {
          toast(again.message, true);
          return null;
        }
      }
    }
  }
}

async function loadTwoFactor() {
  try {
    renderTwoFactor(await api("/api/auth/2fa"));
  } catch (err) {
    $("#twofa-state").textContent = "?";
    $("#twofa-detail").textContent = err.message;
  }
}

function renderTwoFactor(status) {
  const stateEl = $("#twofa-state");
  stateEl.textContent = status.enabled ? "ON" : "OFF";
  stateEl.className = status.enabled ? "twofa-on" : "twofa-off";
  const left = status.recovery_codes_left;
  $("#twofa-detail").textContent = status.enabled
    ? `On since ${formatDate(status.enabled_at)}. ${left} recovery code${left === 1 ? "" : "s"} left` +
      (left <= 3 ? ": running low, make new ones." : ".")
    + (status.required ? " Required on this server." : "")
    : status.required
      ? "Required on this server."
      : "Off: anyone who learns your password can log in as you.";
  // While the server requires 2FA there is no way to turn it off: a new
  // phone swaps the authenticator in one step instead.
  const actions = status.enabled
    ? [
      h("button", { type: "button", class: "btn", onclick: replaceAuthenticator }, "[ NEW PHONE ]"),
      h("button", { type: "button", class: "btn", onclick: newRecoveryCodes }, "[ NEW RECOVERY CODES ]"),
      status.required ? null
        : h("button", { type: "button", class: "btn danger", onclick: disableTwoFactor }, "[ TURN OFF ]"),
    ].filter(Boolean)
    : [h("button", { type: "button", class: "btn btn-primary", onclick: setUpTwoFactor }, "[ SET UP ]")];
  $("#twofa-actions").replaceChildren(...actions);
}

async function setUpTwoFactor() {
  const account = await enrollTwoFactor();
  if (!account) return;
  toast("Two-factor login is on; other browsers were logged off");
  loadTwoFactor();
}

async function newRecoveryCodes() {
  const res = await askUntil({
    title: "NEW RECOVERY CODES",
    text: "Your current recovery codes stop working and ten new ones are made.",
    fields: [PASSWORD_FIELD, SECOND_FACTOR_FIELD],
    submit: "[ MAKE NEW CODES ]",
    validate: reauthProblem,
  }, (values) => api("/api/auth/2fa/recovery-codes", { method: "POST", body: values }));
  if (!res) return;
  await showRecoveryCodes(res.recovery_codes, "NEW RECOVERY CODES", state.user.username);
  loadTwoFactor();
}

async function disableTwoFactor() {
  const res = await askUntil({
    title: "TURN OFF TWO-FACTOR LOGIN?",
    text: "Logging in will only need your password again. Your recovery codes stop working.",
    fields: [PASSWORD_FIELD, SECOND_FACTOR_FIELD],
    submit: "[ TURN OFF ]",
    danger: true,
    validate: reauthProblem,
  }, (values) => api("/api/auth/2fa/disable", { method: "POST", body: values }));
  if (!res) return;
  toast("Two-factor login is off");
  loadTwoFactor();
}

/**
 * Move to a new phone: scan a new QR code, then confirm with a code from
 * the old app (or a recovery code) and one from the new app. The old app
 * stops working only once the new one is proven, so the account is never
 * left without a second factor.
 */
async function replaceAuthenticator() {
  let setup;
  try {
    setup = await api("/api/auth/2fa/setup", { method: "POST" });
  } catch (err) {
    toast(err.message, true);
    return;
  }
  let problem = "";
  for (;;) {
    const values = await askDialog({
      error: problem,
      title: "MOVE TO A NEW PHONE",
      text: "1. Scan this QR code with the authenticator app on your new phone. " +
        "2. Enter a code from your OLD app (lost it? use a recovery code), a code from the NEW app, and your password. " +
        "The old app stops working; your recovery codes stay. Other browsers logged in as you will be logged off.",
      extra: twoFactorSetupView(setup),
      fields: [
        { ...SECOND_FACTOR_FIELD, label: "OLD CODE" },
        { name: "new_code", label: "NEW CODE", inputmode: "numeric", autocomplete: "one-time-code", maxlength: 7, placeholder: "6 digits" },
        PASSWORD_FIELD,
      ],
      submit: "[ SWITCH ]",
      validate: (v) => (!v.code.trim() ? "Enter a code from your old app, or a recovery code."
        : !/^\d{6}$/.test(v.new_code.replace(/[\s-]/g, "")) ? "Enter the 6-digit code your new app shows."
          : !v.password ? "Enter your password." : null),
    });
    if (!values) return;
    try {
      await api("/api/auth/2fa/replace", { method: "POST", body: values });
      toast("New phone set up; other browsers were logged off");
      loadTwoFactor();
      return;
    } catch (err) {
      problem = err.message;
      // The QR code was left open too long: show a new one.
      if (err.status === 409) {
        try {
          setup = await api("/api/auth/2fa/setup", { method: "POST" });
          problem = "That QR code expired. Remove it from your new app if you added it, then scan this new one.";
        } catch (again) {
          toast(again.message, true);
          return;
        }
      }
    }
  }
}

/**
 * Enter the app, unless the server says this account's password no longer
 * meets the policy: then the session can do nothing but change it, so ask
 * for a new one first (or log off).
 */
async function startSession(user, knownPassword = null) {
  // The password just typed at the gate, if any; null asks for it.
  let password = knownPassword;
  let problem = "";
  while (user.password_change_required) {
    const temporary = user.password_temporary;
    const values = await askDialog({
      error: problem,
      title: temporary ? "CHOOSE YOUR PASSWORD" : "NEW PASSWORD REQUIRED",
      text: (temporary
        ? `Welcome aboard, ${user.username}. You logged in with a temporary password from a captain; ` +
          "choose your own to continue"
        : "Your password is too weak for this server. Choose a stronger one to continue") +
        `: at least ${MIN_PASSWORD} characters; a few unrelated words work well.` +
        (user.two_factor_enrollment_required ? " Next, you will set up two-factor login." : ""),
      fields: [
        password ? null : { name: "current", label: "CURRENT", type: "password", autocomplete: "current-password" },
        { name: "new", label: "NEW", type: "password", autocomplete: "new-password" },
        { name: "confirm", label: "CONFIRM", type: "password", autocomplete: "new-password" },
      ].filter(Boolean),
      submit: "[ CHANGE ]",
      cancel: "[ LOG OFF ]",
      validate: (v) => (!password && !v.current ? "Enter your current password." : newPasswordProblem(v.new, v.confirm)),
    });
    if (!values) { await logOff(); return; }
    const current = password ?? values.current;
    try {
      user = await api("/api/auth/password", { method: "POST", body: { current, new: values.new } });
      password = values.new;
    } catch (err) {
      problem = err.message;
      // The remembered password was refused: ask for it after all.
      if (err.status === 401) password = null;
    }
  }
  // Two-factor login is required for this account and not set up yet.
  if (user.two_factor_enrollment_required) {
    user = await enrollTwoFactor({ forced: true, password });
    if (!user) { await logOff(); return; }
  }
  enterApp(user);
}

async function logOff() {
  try { await api("/api/auth/logout", { method: "POST" }); } catch { /* reload anyway */ }
  location.reload();
}

async function changeOwnPassword() {
  const values = await askDialog({
    title: "CHANGE PASSWORD",
    text: "Other browsers logged in as you will be logged off.",
    fields: [
      { name: "current", label: "CURRENT", type: "password", autocomplete: "current-password" },
      { name: "new", label: "NEW", type: "password", autocomplete: "new-password" },
      { name: "confirm", label: "CONFIRM", type: "password", autocomplete: "new-password" },
    ],
    submit: "[ CHANGE ]",
    validate: (v) => (!v.current ? "Enter your current password." : newPasswordProblem(v.new, v.confirm)),
  });
  if (!values) return;
  try {
    await api("/api/auth/password", { method: "POST", body: { current: values.current, new: values.new } });
    toast("Password changed; other sessions were logged off");
  } catch (err) {
    toast(err.message, true);
  }
}

// ---------- admin: server rules (captain only) ----------

const rules = { current: null };

const POLICY_NOTES = {
  all: "Every account needs a code from an authenticator app to log in, and nobody can turn it off. " +
    "Accounts without it must set it up right after logging in.",
  captains: "Captains need an authenticator code to log in and cannot turn it off; pirates choose for themselves.",
  off: "Everyone chooses for themselves. Anyone who learns a password without 2FA can log in as that user.",
};
const POLICY_NAMES = { all: "REQUIRED FOR EVERYONE", captains: "CAPTAINS ONLY", off: "OPTIONAL" };
const POLICY_STRENGTH = { off: 0, captains: 1, all: 2 };

const twoFactorRequiredFor = (role) => Boolean(rules.current) &&
  (rules.current.two_factor === "all" || (rules.current.two_factor === "captains" && role === "captain"));

/** `m:ss`, `h:mm:ss` or plain minutes to seconds; null if unreadable. */
function parseRuleLength(text) {
  const parts = text.trim().split(":");
  if (parts.length > 3 || parts.some((p) => !/^\d+$/.test(p))) return null;
  if (parts.length === 1) return Number(parts[0]) * 60;
  return parts.reduce((total, p) => total * 60 + Number(p), 0);
}

async function loadRules() {
  try {
    renderRules(await api("/api/admin/settings"));
  } catch (err) {
    $("#rule-2fa-detail").textContent = err.message;
  }
}

function renderRules(settings) {
  rules.current = settings;
  for (const chip of document.querySelectorAll("#rule-2fa .chip")) {
    const on = chip.dataset.policy === settings.two_factor;
    chip.classList.toggle("active", on);
    chip.setAttribute("aria-checked", String(on));
  }
  $("#rule-2fa-detail").textContent = POLICY_NOTES[settings.two_factor] || "";
  if (document.activeElement !== $("#rule-issuer")) $("#rule-issuer").value = settings.totp_issuer;
  if (document.activeElement !== $("#rule-length")) {
    $("#rule-length").value = settings.max_track_secs ? formatDuration(settings.max_track_secs) : "0";
  }
  $("#crew-form-note").textContent = "Inkult generates a temporary password for you to hand over. At their first " +
    "login they must choose their own password" +
    (settings.two_factor === "all" ? ", then set up two-factor login." : ".");
}

/** Save a change to the server rules; resolves to whether it was saved. */
async function saveRules(change, done) {
  try {
    renderRules(await api("/api/admin/settings", { method: "PATCH", body: change }));
    toast(done);
    loadCrew();
    return true;
  } catch (err) {
    toast(err.message, true);
    return false;
  }
}

async function pickTwoFactorRule(policy) {
  const current = rules.current;
  if (!current || policy === current.two_factor) return;
  const name = POLICY_NAMES[policy];
  let mine;
  try {
    mine = await api("/api/auth/2fa");
  } catch (err) {
    toast(err.message, true);
    return;
  }
  // The new rule requires 2FA from us, and we have not set it up.
  const strandsMe = POLICY_STRENGTH[policy] > 0 && !mine.enabled;
  let saved;
  if (POLICY_STRENGTH[policy] < POLICY_STRENGTH[current.two_factor]) {
    // Relaxing it is what a stolen session would do: prove it is you.
    const res = await askUntil({
      title: `TWO-FACTOR LOGIN: ${name}?`,
      text: POLICY_NOTES[policy] + " Accounts that have 2FA keep it; it just becomes possible to turn it off. " +
        "Confirm with your password and a code.",
      fields: [PASSWORD_FIELD, SECOND_FACTOR_FIELD],
      submit: "[ RELAX THE RULE ]",
      danger: true,
      validate: reauthProblem,
    }, (values) => api("/api/admin/settings", { method: "PATCH", body: { two_factor: policy, ...values } }));
    if (res) {
      renderRules(res);
      toast(`Two-factor login: ${name.toLowerCase()}`);
      loadCrew();
    }
    saved = Boolean(res);
  } else {
    const ok = await askDialog({
      title: `TWO-FACTOR LOGIN: ${name}?`,
      text: POLICY_NOTES[policy] + " Whoever it now applies to and has not set it up can do nothing else until " +
        "they do, starting with their next click" +
        (strandsMe ? ". That includes you: you will set it up right away" : "") + ".",
      submit: "[ REQUIRE IT ]",
    });
    saved = ok && await saveRules({ two_factor: policy }, `Two-factor login: ${name.toLowerCase()}`);
    // We were just restricted: the next request says so and the gate opens.
    if (saved && strandsMe) location.reload();
  }
  if (!saved) renderRules(current);
}

async function saveIssuer(event) {
  event.preventDefault();
  const issuer = $("#rule-issuer").value.trim();
  if (!issuer) { toast("Enter a name", true); return; }
  if (/[:\u0000-\u001f]/.test(issuer)) { toast("The name can't contain “:”", true); return; }
  if (rules.current && issuer === rules.current.totp_issuer) return;
  if (await saveRules({ totp_issuer: issuer }, `Authenticator name: ${issuer}`)) $("#rule-issuer").blur();
}

async function saveLength(event) {
  event.preventDefault();
  const secs = parseRuleLength($("#rule-length").value);
  if (secs === null) { toast("Enter a length like 10:00, or 0 for no limit", true); return; }
  if (secs > 86400) { toast("At most 24:00:00 (0 = no limit)", true); return; }
  if (rules.current && secs === rules.current.max_track_secs) return;
  const done = secs ? `Tracks over ${formatDuration(secs)} are now left out` : "No track length limit";
  if (await saveRules({ max_track_secs: secs }, done)) $("#rule-length").blur();
}

// ---------- crew (captain only) ----------

const RANKS = { captain: "CAPTAIN", pirate: "PIRATE" };

function formatDate(secs) {
  const d = new Date(secs * 1000);
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")}`;
}

async function loadCrew() {
  const container = $("#crew");
  if (!container.childElementCount) container.replaceChildren(...skeleton(3));
  try {
    renderCrew(await api("/api/users"));
  } catch (err) {
    container.replaceChildren(h("p", { class: "error-line" }, err.message));
  }
}

/** Run a crew change, report it, and refresh the list. */
async function crewAction(run, done, affectsSelf = false) {
  try {
    await run();
    toast(done);
    // Our own rank changed: the whole UI depends on it.
    if (affectsSelf) location.reload();
    else loadCrew();
  } catch (err) {
    toast(err.message, true);
  }
}

function crewRow(member) {
  const self = member.id === state.user.id;
  const promote = member.role === "pirate";
  const locked = Boolean(member.locked_at);
  const actions = [
    locked ? h("button", {
      type: "button", class: "btn danger",
      title: "Locked after too many failed logins",
      onclick: () => crewAction(
        () => api(`/api/users/${member.id}`, { method: "PATCH", body: { unlock: true } }),
        `${member.username} can log in again`),
    }, "[ UNLOCK ]") : null,
    h("button", {
      type: "button", class: "btn",
      onclick: () => crewAction(
        () => api(`/api/users/${member.id}`, { method: "PATCH", body: { role: promote ? "captain" : "pirate" } }),
        `${member.username} is now a ${promote ? "captain" : "pirate"}`, self),
    }, promote ? "[ PROMOTE ]" : "[ DEMOTE ]"),
    member.two_factor_enabled && !self ? h("button", {
      type: "button", class: "btn",
      onclick: async () => {
        const ok = await askDialog({
          title: `RESET 2FA FOR ${member.username.toUpperCase()}?`,
          text: "For a lost phone AND lost recovery codes (with recovery codes they can switch phones themselves " +
            "from SETTINGS → [NEW PHONE]): their authenticator and recovery codes stop working and they are logged off. " +
            "Their password stays; at their next login they set up two-factor login again" +
            (rules.current && rules.current.two_factor !== "off" ? " (if the server requires it, before anything else)" : "") +
            ". Make sure it is really them asking.",
          submit: "[ RESET 2FA ]",
          danger: true,
        });
        if (ok) {
          crewAction(() => api(`/api/users/${member.id}`, { method: "PATCH", body: { reset_2fa: true } }),
            `Two-factor login removed from ${member.username}`);
        }
      },
    }, "[ RESET 2FA ]") : null,
    // Your own password is changed from [ PASSWD ], which asks for it.
    self ? null : h("button", {
      type: "button", class: "btn",
      onclick: async () => {
        const ok = await askDialog({
          title: `RESET PASSWORD FOR ${member.username.toUpperCase()}?`,
          text: "Inkult generates a temporary password for you to hand over; their current one stops working " +
            "and their sessions are logged off. At their next login they must choose their own." +
            (locked ? " This also unlocks the account." : ""),
          submit: "[ RESET ]",
          danger: true,
        });
        if (!ok) return;
        let res;
        try {
          res = await api(`/api/users/${member.id}`, { method: "PATCH", body: { reset_password: true } });
        } catch (err) {
          toast(err.message, true);
          return;
        }
        loadCrew();
        await showTemporaryPassword(member.username, res.temporary_password,
          "Give it to them privately. At their next login they must replace it with their own. " +
          "It works for 3 days and is shown only now.");
      },
    }, "[ RESET PW ]"),
    self ? null : h("button", {
      type: "button", class: "btn danger",
      onclick: async () => {
        const ok = await askDialog({
          title: `WALK ${member.username.toUpperCase()} OFF THE PLANK?`,
          text: `The account is deleted and logged off. Its ${member.tracks} track(s) stay in the hold for anyone else who owns them; no files are deleted.`,
          submit: "[ DELETE ACCOUNT ]",
          danger: true,
        });
        if (ok) {
          crewAction(() => api(`/api/users/${member.id}`, { method: "DELETE" }), `${member.username} walked the plank`);
        }
      },
    }, "[ PLANK ]"),
  ];
  return h("div", { class: "crew-row" },
    h("div", { class: "title-cell" },
      h("div", { class: "title" }, member.username, self ? h("span", { class: "dim" }, " (you)") : null),
      h("div", { class: "subtitle" }, `joined ${formatDate(member.created_at)}`,
        member.two_factor_enabled ? h("span", { class: "twofa-tag" }, "  2FA")
          : member.two_factor_pending ? h("span", { class: "twofa-tag" }, "  2FA PENDING") : null,
        member.password_temporary
          ? (member.temporary_password_expires_at * 1000 <= Date.now()
            ? h("span", { class: "locked" }, "  TEMP PASSWORD EXPIRED: [RESET PW] FOR A NEW ONE")
            : h("span", { class: "twofa-tag" }, "  TEMP PASSWORD")) : null,
        locked ? h("span", { class: "locked" }, `  LOCKED ${formatDate(member.locked_at)}: too many failed logins`) : null)),
    h("div", { class: member.role === "captain" ? "rank captain" : "rank" }, RANKS[member.role] || member.role),
    h("div", { class: "crew-tracks" }, `${member.tracks} FILES`),
    h("div", { class: "crew-actions" }, actions));
}

function renderCrew(members) {
  $("#crew").replaceChildren(
    h("div", { class: "crew-list" },
      h("div", { class: "crew-row list-head", "aria-hidden": "true" },
        h("span", {}, "HANDLE"), h("span", {}, "RANK"), h("span", { class: "crew-tracks" }, "LIBRARY"),
        h("span", { class: "crew-actions" })),
      ...members.map(crewRow)),
    listFoot(`${members.length} ABOARD   ${members.filter((m) => m.role === "captain").length} CAPTAIN(S)`));
}

/** Show a generated password to the captain, the only time it is shown. */
async function showTemporaryPassword(username, password, text) {
  const copy = navigator.clipboard && window.isSecureContext
    ? h("button", {
      type: "button", class: "btn",
      onclick: async (event) => {
        try {
          await navigator.clipboard.writeText(password);
          event.target.textContent = "[ COPIED ]";
        } catch {
          event.target.textContent = "[ COPY FAILED: SELECT IT ]";
        }
      },
    }, "[ COPY ]")
    : null;
  await askDialog({
    title: `TEMPORARY PASSWORD FOR ${username.toUpperCase()}`,
    text,
    extra: h("div", {}, h("code", { class: "twofa-key", "aria-label": "Temporary password" }, password), copy),
    submit: "[ DONE ]",
    cancel: null,
  });
}

async function signOn(event) {
  event.preventDefault();
  const username = $("#crew-user").value.trim();
  if (!username) { toast("Enter a handle", true); return; }
  let account;
  try {
    account = await api("/api/users", { method: "POST", body: { username, role: state.crewRole } });
  } catch (err) {
    toast(err.message, true);
    return;
  }
  $("#crew-user").value = "";
  loadCrew();
  await showTemporaryPassword(account.user.username, account.temporary_password,
    `${account.user.username} is signed on as ${account.user.role}. Give them this password privately ` +
    "(in person, or over a channel only they can read). At their first login they must choose their own " +
    "password" + (twoFactorRequiredFor(account.user.role) ? ", then set up two-factor login with an authenticator app" : "") +
    ". It works for 3 days and is shown only now; if it gets lost or expires, use [RESET PW].");
}

/** Make a group of `.chip` radio buttons exclusive; `onPick(button)` on change. */
function chipGroup(selector, onPick) {
  const chips = Array.from(document.querySelectorAll(selector));
  for (const chip of chips) {
    chip.addEventListener("click", () => {
      for (const other of chips) {
        other.classList.toggle("active", other === chip);
        other.setAttribute("aria-checked", String(other === chip));
      }
      onPick(chip);
    });
  }
}

// ---------- wiring ----------

/** Show the app for a logged-in user; wires everything up the first time. */
function enterApp(user) {
  state.user = user;
  $("#gate").hidden = true;
  $("#app").hidden = false;
  closePanel({ restoreFocus: false });
  $("#user-name").textContent = user.username;
  $("#user-rank").textContent = RANKS[user.role] || user.role;
  const captain = isCaptain();
  $('.nav-item[data-view="admin"]').hidden = !captain;
  $("#library-scope").hidden = !captain;
  $("#jobs-scope").hidden = !captain;
  if (!state.started) {
    state.started = true;
    tickClock();
    setInterval(tickClock, 1000);
    setupKult();
    setInterval(loadOnline, ONLINE_POLL_MS);
    setupPlayer();
    setupSoulseek();
  }
  loadStatus();
  loadOnline();
  slskSchedule(0);
  roomsSchedule(0);
  showView("search");
  $("#search-input").focus();
}

async function boot() {
  let auth;
  try {
    auth = await api("/api/auth/state");
  } catch (err) {
    showGate("login", `Cannot reach Inkult: ${err.message}`);
    return;
  }
  $("#gate-version").textContent = auth.version;
  if (auth.setup_required) {
    showGate("setup");
    return;
  }
  let user;
  try {
    user = await api("/api/auth/me");
  } catch {
    showGate("login");
    return;
  }
  if (user.password_change_required || user.two_factor_enrollment_required) showGate("login");
  await startSession(user);
}

document.addEventListener("DOMContentLoaded", () => {
  setupLogoFire();
  setupPanels();
  for (const btn of document.querySelectorAll(".nav-item")) {
    btn.addEventListener("click", () => showView(btn.dataset.view));
  }
  chipGroup("#view-search .chip", (chip) => {
    state.provider = chip.dataset.provider;
    if ($("#search-input").value.trim() && !looksLikeUrl($("#search-input").value)) {
      $("#search-form").requestSubmit();
    }
  });
  state.librarySort = storedLibrarySort();
  renderLibrarySort();
  for (const chip of document.querySelectorAll("#library-sort .chip")) {
    chip.addEventListener("click", () => {
      const sort = state.librarySort;
      state.librarySort = chip.dataset.sort === sort.by
        ? { by: sort.by, reversed: !sort.reversed }
        : { by: chip.dataset.sort, reversed: false };
      rememberLibrarySort();
      renderLibrarySort();
      renderLibrary();
    });
  }
  chipGroup("#library-scope .chip", (chip) => {
    state.libraryScope = chip.dataset.scope;
    state.library = [];
    state.librarySelected.clear();
    state.libraryAnchor = null;
    loadLibrary();
  });
  chipGroup("#jobs-scope .chip", (chip) => {
    state.jobsScope = chip.dataset.scope;
    // A different set of jobs: their states are not transitions.
    state.jobStates = null;
    pollJobs();
  });
  chipGroup(".crew-rank .chip", (chip) => { state.crewRole = chip.dataset.role; });
  for (const chip of document.querySelectorAll("#rule-2fa .chip")) {
    chip.addEventListener("click", () => pickTwoFactorRule(chip.dataset.policy));
  }
  $("#rule-issuer-form").addEventListener("submit", saveIssuer);
  $("#rule-length-form").addEventListener("submit", saveLength);
  chipGroup("#theme-options .chip", (chip) => {
    applyTheme(chip.dataset.themeId);
    renderSettings();
  });
  chipGroup("#stars-options .chip", (chip) => {
    applyStars(chip.dataset.stars === "on");
    renderSettings();
  });
  chipGroup("#kult-options .chip", (chip) => {
    applyKult(chip.dataset.kult === "on");
    renderSettings();
  });
  chipGroup("#cursor-options .chip", (chip) => {
    applyCursor(chip.dataset.cursorId);
    syncCursorFx();
    renderSettings();
  });
  $("#gate-form").addEventListener("submit", submitGate);
  $("#gate-back").addEventListener("click", () => showGate("login"));
  // Most apps show exactly six digits: submit as soon as they are in.
  $("#gate-otp").addEventListener("input", (event) => {
    if (/^\d{6}$/.test(event.target.value.replace(/[\s-]/g, "")) && !$("#gate-submit").disabled) {
      $("#gate-form").requestSubmit();
    }
  });
  $("#crew-form").addEventListener("submit", signOn);
  $("#btn-logout").addEventListener("click", logOff);
  $("#btn-passwd").addEventListener("click", changeOwnPassword);
  $("#search-form").addEventListener("submit", runSearch);
  for (const input of document.querySelectorAll(".search-bar > input[placeholder]")) marqueePlaceholder(input);
  setupSearchFilters();
  $("#library-filter").addEventListener("input", renderLibrary);
  $("#clear-finished").addEventListener("click", async () => {
    try { await api(`/api/downloads/finished?scope=${jobsScope()}`, { method: "DELETE" }); await pollJobs(); }
    catch (err) { toast(err.message, true); }
  });
  $("#room-new").addEventListener("submit", roomsCreate);
  $("#room-join-private").addEventListener("click", roomsJoinPrivate);
  // BBS menu keys: 1–7 switch areas unless typing, a modifier is held, or
  // nobody is logged in.
  const VIEW_KEYS = { 1: "search", 2: "soulseek", 3: "downloads", 4: "library", 5: "messages", 6: "settings", 7: "admin" };
  document.addEventListener("keydown", (event) => {
    const view = VIEW_KEYS[event.key];
    if (!view || !state.user || event.ctrlKey || event.metaKey || event.altKey || openMenu) return;
    if ($("#dialog").open) return;
    const target = event.target;
    if (target instanceof Element && target.closest("input, textarea, select, [contenteditable]")) return;
    if (view === "admin" && !isCaptain()) return;
    event.preventDefault();
    showView(view);
  });
  setupCursorFx();
  setupScrollbars();
  boot();
});
