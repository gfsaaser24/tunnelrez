// TunnelRez panel. Plain JS, no framework. Talks to the Rust side through window.__TAURI__ (withGlobalTauri).
// Opened outside the app (no __TAURI__), it runs on a mock backend: `index.html?theme=light#streaming` renders
// one state for screenshots without starting anything.
"use strict";

const $ = (id) => document.getElementById(id);
const TAURI = window.__TAURI__;
const MOCK = !TAURI;

// ------------------------------------------------------------------------------------------------ words

const PICTURES = { "1080p60": "1080p60", "4k60": "4K60" };
const CODECS = [
  { value: "auto", label: "Auto" },
  { value: "hevc", label: "HEVC" },
  { value: "h264", label: "H.264" },
  { value: "av1", label: "AV1" },
];
const ENCODERS = {
  auto: { label: "Auto", detail: "Best that works", short: "Auto",
    tip: "Picks the best encoder that works on this PC: NVIDIA NVENC native first, the FFmpeg paths as fallback." },
  nvenc_native: { label: "NVIDIA NVENC native", detail: "In-process, lowest latency", short: "NVIDIA NVENC native",
    tip: "Captures and encodes inside TunnelRez, no FFmpeg. Lowest latency; the path used for the TV tests." },
  nvenc: { label: "NVIDIA NVENC (FFmpeg)", detail: "Fallback", short: "NVIDIA NVENC (FFmpeg)",
    tip: "NVIDIA GPU encoder through FFmpeg. Fallback; higher latency than native." },
  qsv: { label: "Intel Quick Sync (FFmpeg)", detail: "Fallback", short: "Intel Quick Sync (FFmpeg)",
    tip: "Intel GPU encoder through FFmpeg. Fallback; higher latency." },
  amf: { label: "AMD AMF (FFmpeg)", detail: "Fallback", short: "AMD AMF (FFmpeg)",
    tip: "AMD GPU encoder through FFmpeg. Fallback; higher latency." },
  mf: { label: "Windows Media Foundation (FFmpeg)", detail: "Fallback", short: "Media Foundation (FFmpeg)",
    tip: "The GPU's encoder through Windows' own Media Foundation, via FFmpeg. Fallback; higher latency. HEVC needs the HEVC Video Extensions." },
  cpu: { label: "CPU x264 / x265 / SVT-AV1 (FFmpeg)", detail: "Any PC, slowest", short: "CPU (FFmpeg)",
    tip: "Software encoders through FFmpeg. Fallback; higher latency and high CPU use; HEVC and AV1 may not keep up at 4K60." },
};
/**
 * Bitrate fallbacks until the first snapshot; the real numbers come from `snapshot.bitrate` (the Rust table).
 * `settings.bitrate_mbps` is `"default"` (the picture's own rate), `"auto"`, or the Mb/s the user picked.
 */
const BITRATE = { min: 10, max: 150, def: 40 };
const PHASES = {
  checking: "Looking for your TV…",
  plugging: "Adding the TV screen…",
  starting: "Starting the stream…",
  reconnecting: "Connection lost. Reconnecting…",
  restarting: "The stream stopped. Starting it again…",
};
const G = { extend: "", mirror: "", stop: "", retry: "", open: "", ok: "",
  warn: "", bad: "", chev: "", pin: "\uE718", pinned: "\uE842" };
const PIN_TIP = {
  true: "Keep open: on. Drag the top to move it.",
  false: "Keep open: off. Hides when you click away.",
};

/** The picture really streamed (from the sender's stats), or the picked one until the first stats arrive. */
function pictureWords(s) {
  return s.stats?.picture ?? PICTURES[s.settings.picture];
}

/** "Second screen · 4K60 · HEVC · NVENC" */
function streamingSub(s) {
  const where = s.settings.mode === "extend" ? "Second screen" : `Mirroring ${currentMonitor(s)?.label ?? "this screen"}`;
  const enc = encoderWords(s.stats?.encoder);
  const what = enc && s.settings.encoder === "auto" ? `Auto: ${enc}` : enc;
  return [where, pictureWords(s), what].filter(Boolean).join(" · ");
}

/** 3840×2160 is "4K"; other sizes as "2560x1440". */
function sizeWords(w, h) {
  return w === 3840 && h === 2160 ? "4K" : `${w}x${h}`;
}

function latencyClass(ms) {
  if (ms == null) return "";
  return ms < 30 ? "lat-good" : ms < 60 ? "lat-warn" : "lat-bad";
}

const ENCODER_NAMES = { nvenc: "NVIDIA NVENC (FFmpeg)", qsv: "Intel Quick Sync (FFmpeg)", amf: "AMD AMF (FFmpeg)",
  mf: "Windows Media Foundation (FFmpeg)", x264: "CPU x264 (FFmpeg)", x265: "CPU x265 (FFmpeg)",
  svtav1: "CPU SVT-AV1 (FFmpeg)", libx264: "CPU x264 (FFmpeg)", libx265: "CPU x265 (FFmpeg)",
  libsvtav1: "CPU SVT-AV1 (FFmpeg)", native: "NVIDIA NVENC native" };

/**
 * The sender's `codec_out` -> panel words. Formats: "<codec>/<backend> <path>" (ffmpeg backends, e.g.
 * "hevc/nvenc zero-copy", "h264/x264 copy"), "<codec>/native", and the older "<codec>/ffmpeg <backend>".
 * The backend is the first known encoder name; the path words (copy, zero-copy, copy+scale_cuda) never are.
 */
function encoderWords(label) {
  if (!label) return "";
  const [codec, rest = ""] = label.split("/");
  const c = CODECS.find((x) => x.value === codec)?.label ?? codec.toUpperCase();
  const t = rest.split(/\s+/).filter(Boolean);
  const known = t.find((w) => ENCODER_NAMES[w]);
  const e = known ? ENCODER_NAMES[known] : (t.find((w) => w !== "ffmpeg") ?? "").toUpperCase();
  return e ? `${c} · ${e}` : c;
}

/** `encoderWords` against every label format the sender emits (run in the mock: `?mock`, see the console). */
function checkEncoderWords() {
  const cases = [
    ["hevc/nvenc zero-copy", "HEVC · NVIDIA NVENC (FFmpeg)"], ["h264/x264 copy", "H.264 · CPU x264 (FFmpeg)"],
    ["hevc/nvenc copy+scale_cuda", "HEVC · NVIDIA NVENC (FFmpeg)"], ["hevc/qsv copy", "HEVC · Intel Quick Sync (FFmpeg)"],
    ["hevc/native", "HEVC · NVIDIA NVENC native"], ["hevc/ffmpeg nvenc", "HEVC · NVIDIA NVENC (FFmpeg)"],
    ["hevc/ffmpeg x265", "HEVC · CPU x265 (FFmpeg)"], ["hevc/mf copy", "HEVC · Windows Media Foundation (FFmpeg)"],
  ];
  for (const [label, want] of cases) {
    const got = encoderWords(label);
    if (got !== want) console.error(`encoderWords(${JSON.stringify(label)}) = ${JSON.stringify(got)}, want ${JSON.stringify(want)}`);
  }
}

function fmt(v, digits = 0) {
  return v == null || Number.isNaN(v) ? "--" : v.toFixed(digits);
}

// ------------------------------------------------------------------------------------------------ backend

if (MOCK) checkEncoderWords();

const api = MOCK ? makeMock() : {
  invoke: (cmd, args) => TAURI.core.invoke(cmd, args),
  listen: (ev, fn) => TAURI.event.listen(ev, (e) => fn(e.payload)),
};

// ------------------------------------------------------------------------------------------------ theme

const params = new URLSearchParams(location.search);
const forcedTheme = params.get("theme");
const mqLight = matchMedia("(prefers-color-scheme: light)");
function applyTheme() {
  document.documentElement.dataset.theme = forcedTheme || (mqLight.matches ? "light" : "dark");
  drawSpark();
}
mqLight.addEventListener("change", applyTheme);

// ------------------------------------------------------------------------------------------------ controls

/** Segmented control (a radio group with a sliding thumb). */
function segmented(el, onPick) {
  const ctl = { el, value: null, options: [] };
  el.addEventListener("click", (e) => {
    const b = e.target.closest("button[role=radio]");
    if (b && !b.disabled && b.dataset.value !== ctl.value) onPick(b.dataset.value);
  });
  el.addEventListener("keydown", (e) => {
    const keys = { ArrowRight: 1, ArrowDown: 1, ArrowLeft: -1, ArrowUp: -1 };
    if (!(e.key in keys)) return;
    e.preventDefault();
    const btns = [...el.querySelectorAll("button[role=radio]:not(:disabled)")];
    const i = btns.findIndex((b) => b.dataset.value === ctl.value);
    const next = btns[(i + keys[e.key] + btns.length) % btns.length];
    if (next) {
      onPick(next.dataset.value);
      next.focus();
    }
  });
  ctl.set = (options, value) => {
    const sig = JSON.stringify(options);
    if (sig !== ctl.sig) {
      ctl.sig = sig;
      el.querySelectorAll("button[role=radio]").forEach((b) => b.remove());
      let thumb = el.querySelector(".thumb");
      if (!thumb) {
        thumb = Object.assign(document.createElement("i"), { className: "thumb" });
        thumb.setAttribute("aria-hidden", "true");
        el.append(thumb);
      }
      for (const o of options) {
        const b = document.createElement("button");
        b.type = "button";
        b.setAttribute("role", "radio");
        b.dataset.value = o.value;
        if (o.glyph) b.innerHTML = `<span class="glyph" aria-hidden="true">${o.glyph}</span>`;
        b.append(o.label);
        b.disabled = !!o.disabled;
        if (o.title) b.title = o.title;
        el.insertBefore(b, thumb);
      }
    }
    ctl.value = value;
    const btns = [...el.querySelectorAll("button[role=radio]")];
    btns.forEach((b) => {
      const on = b.dataset.value === value;
      b.setAttribute("aria-checked", String(on));
      b.tabIndex = on ? 0 : -1;
    });
    if (!btns.some((b) => b.tabIndex === 0) && btns[0]) btns[0].tabIndex = 0;
    placeThumb(el);
  };
  return ctl;
}

function placeThumb(el) {
  const thumb = el.querySelector(".thumb");
  const on = el.querySelector("button[aria-checked=true]");
  if (!thumb) return;
  if (!on) {
    thumb.style.opacity = "0";
    return;
  }
  thumb.style.opacity = "1";
  thumb.style.width = `${on.offsetWidth}px`;
  thumb.style.transform = `translateX(${on.offsetLeft - 2}px)`;
}

/** Combo box: a button that opens a list popup (Fluent ComboBox). */
function select(el, onPick) {
  const btn = document.createElement("button");
  btn.type = "button";
  btn.setAttribute("aria-haspopup", "listbox");
  btn.setAttribute("aria-expanded", "false");
  btn.setAttribute("aria-label", el.dataset.label ?? "");
  btn.innerHTML = `<span class="name"></span><span class="sub"></span><span class="glyph chev" aria-hidden="true">${G.chev}</span>`;
  el.append(btn);
  const ctl = { el, btn, options: [], value: null };
  btn.addEventListener("click", () => openPopup(ctl, onPick));
  btn.addEventListener("keydown", (e) => {
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      openPopup(ctl, onPick);
    }
  });
  ctl.set = (options, value) => {
    ctl.options = options;
    ctl.value = value;
    const cur = options.find((o) => o.value === value) ?? options[0];
    btn.querySelector(".name").textContent = cur?.label ?? "";
    btn.querySelector(".sub").textContent = cur?.detail ?? "";
    btn.disabled = options.length === 0;
  };
  return ctl;
}

let popupFor = null;
function openPopup(ctl, onPick) {
  const pop = $("popup");
  if (popupFor === ctl) return closePopup();
  popupFor = ctl;
  pop.innerHTML = "";
  ctl.options.forEach((o, i) => {
    const d = document.createElement("div");
    d.setAttribute("role", "option");
    d.id = `opt-${i}`;
    d.dataset.value = o.value;
    d.setAttribute("aria-selected", String(o.value === ctl.value));
    if (o.disabled) d.setAttribute("aria-disabled", "true");
    if (o.title) d.title = o.title;
    d.innerHTML = `<span></span><span class="sub"></span>`;
    d.firstChild.textContent = o.label;
    d.lastChild.textContent = o.detail ?? "";
    d.addEventListener("click", () => {
      if (o.disabled) return;
      closePopup();
      if (o.value !== ctl.value) onPick(o.value);
    });
    pop.append(d);
  });
  pop.hidden = false;
  ctl.btn.setAttribute("aria-expanded", "true");
  // Place over the button; open upward when there is no room below.
  const panel = $("panel").getBoundingClientRect();
  const r = ctl.btn.getBoundingClientRect();
  pop.style.left = `${r.left - panel.left - 4}px`;
  pop.style.width = `${r.width + 8}px`;
  const h = pop.offsetHeight;
  const below = r.top - panel.top;
  const top = below + h <= panel.height - 8 ? below - 4 : Math.max(8, r.bottom - panel.top - h + 4);
  pop.style.top = `${top}px`;
  let active = Math.max(0, ctl.options.findIndex((o) => o.value === ctl.value));
  const mark = () => [...pop.children].forEach((c, i) => c.classList.toggle("active", i === active));
  mark();
  pop.setAttribute("aria-activedescendant", `opt-${active}`);
  pop.tabIndex = -1;
  pop.focus();
  pop.onkeydown = (e) => {
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      const n = ctl.options.length;
      do active = (active + (e.key === "ArrowDown" ? 1 : -1) + n) % n; while (ctl.options[active].disabled && n > 1);
      mark();
      pop.setAttribute("aria-activedescendant", `opt-${active}`);
    } else if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      pop.children[active]?.click();
    } else if (e.key === "Escape" || e.key === "Tab") {
      e.preventDefault();
      closePopup();
    }
  };
}
function closePopup() {
  const pop = $("popup");
  if (!popupFor) return;
  pop.hidden = true;
  popupFor.btn.setAttribute("aria-expanded", "false");
  popupFor.btn.focus();
  popupFor = null;
}
document.addEventListener("pointerdown", (e) => {
  if (popupFor && !$("popup").contains(e.target) && !popupFor.el.contains(e.target)) closePopup();
});

// ------------------------------------------------------------------------------------------------ model

let snap = null;
let history = [];
let advancedOpen = false;
let editingTv = false;
let opening = false;

const set = (key, value) => api.invoke("set_setting", { key, value });
const LOG_FORMATS = [{ value: "markdown", label: "Markdown" }, { value: "json", label: "JSON" }];

const modeSeg = segmented($("mode"), (v) => set("mode", v));
const pictureSeg = segmented($("picture"), (v) => set("picture", v));
const codecSeg = segmented($("codec"), (v) => set("codec", v));
const monitorSel = select($("monitor"), (v) => set("mirror_monitor", v));
const encoderSel = select($("encoder"), (v) => set("encoder", v));
const audioDevSel = select($("audio-dev"), (v) => set("audio_sink", v));
const logFormatSeg = segmented($("log-format"), (v) => set("log_format", v));

function render(s) {
  snap = s;
  history = s.history ?? [];
  if (s.accent?.length >= 5) {
    document.documentElement.style.setProperty("--accent-dark", s.accent[1]);
    document.documentElement.style.setProperty("--accent-light", s.accent[4]);
  }
  const kind = s.state.kind;
  const first = kind === "first_run";
  $("setup").hidden = !first;
  $("main").hidden = first;
  $("panel").dataset.state = kind;
  if (first) return renderSetup(s);

  const st = s.settings;
  const mode = st.mode;
  const active = kind === "connecting" || kind === "streaming";
  const monitorName = currentMonitor(s)?.label ?? "this screen";

  // Header
  const chip = { idle: "Ready", connecting: "Connecting", streaming: "Live", stopping: "Stopping", error: "Offline" };
  const errChip = { tv_unreachable: "Offline", tv_app_not_open: "TV app closed" };
  $("chip-text").textContent = kind === "error" ? errChip[s.state.error?.code] ?? "Stopped" : chip[kind] ?? "";
  $("metric").hidden = kind !== "streaming";
  $("title").hidden = kind === "streaming";
  $("progress").hidden = !(kind === "connecting" || kind === "stopping");
  let title = "";
  let sub = "";
  if (kind === "idle") {
    title = "Ready to stream";
    sub = mode === "extend" ? "Your TV becomes a second screen." : `Your TV shows ${monitorName}.`;
  } else if (kind === "connecting") {
    const re = s.state.phase === "reconnecting" || s.state.phase === "restarting";
    title = re ? "Reconnecting" : "Connecting";
    sub = PHASES[s.state.phase] ?? "";
  } else if (kind === "streaming") {
    sub = streamingSub(s);
  } else if (kind === "stopping") {
    title = "Stopping";
    sub = mode === "extend" ? "Closing the stream and removing the TV screen." : "Closing the stream.";
  } else if (kind === "error") {
    const e = errorView(s);
    title = e.title;
    sub = e.sub;
  }
  $("title").textContent = title;
  $("subtitle").textContent = sub || " ";
  renderStats(s.stats);

  // Notice (encoder fallback)
  $("notice").hidden = !(s.notice && active);
  $("notice-text").textContent = s.notice ?? "";

  // Actions
  const p = $("primary");
  const sec = $("secondary");
  sec.hidden = true;
  p.disabled = false;
  p.classList.toggle("accent", !active);
  if (active) {
    setPrimary(G.stop, "Stop");
  } else if (kind === "stopping") {
    setPrimary(G.stop, "Stopping…");
    p.disabled = true;
  } else if (kind === "error") {
    const e = errorView(s);
    setPrimary(e.primary.glyph, e.primary.text);
    const fixing = s.tv_monitor?.kind === "working";
    p.disabled = e.primary.act === "vdd_fix" && fixing;
    if (e.secondary && !fixing) {
      sec.hidden = false;
      sec.textContent = e.secondary.text;
    }
  } else {
    setPrimary(mode === "extend" ? G.extend : G.mirror, mode === "extend" ? "Extend to TV" : "Mirror to TV");
    // Never start a combination the encoder check says cannot run.
    p.disabled = !!s.block;
  }

  // Encoder check: why the picked encoder x codec cannot run, and the Store buttons for a missing HEVC codec.
  const gateErr = kind === "error" && s.state.error?.code === "unsupported" ? s.state.error : null;
  const block = !active && kind !== "stopping" ? (s.block ?? null) : null;
  const shown = block ?? (gateErr ? { message: "", hevc_store: gateErr.hevc_store } : null);
  $("gate").hidden = !(block || gateErr?.hevc_store);
  $("gate-text").textContent = shown?.hevc_store ? "" : block?.message ?? "";
  $("gate-text").hidden = !$("gate-text").textContent;
  $("gate-store").hidden = !shown?.hevc_store;
  $("gate-checking").hidden = !(s.caps_checking && !active);

  // Mode + monitor
  modeSeg.set([
    { value: "extend", label: "Extend", glyph: G.extend },
    { value: "mirror", label: "Mirror", glyph: G.mirror },
  ], mode);
  $("monitor-row").hidden = mode !== "mirror";
  const mons = s.monitors.map((m) => ({ value: m.device, label: m.label, detail: m.detail }));
  monitorSel.set(mons.length ? mons : [{ value: "", label: "Looking for screens…", disabled: true }],
    currentMonitor(s)?.device ?? "");

  // Picture
  pictureSeg.set(s.pictures.map((v) => ({ value: v, label: PICTURES[v] ?? v })), st.picture);

  // Advanced
  const blocked = s.blocked ?? {};
  codecSeg.set(CODECS.map((c) => {
    const b = c.value !== st.codec && blocked[`${st.encoder}|${c.value}`];
    return b ? { ...c, disabled: true, title: b.message } : c;
  }), st.codec);
  encoderSel.set(s.encoders.map((v) => {
    const b = v !== st.encoder && blocked[`${v}|${st.codec}`];
    const o = { value: v, ...ENCODERS[v], title: ENCODERS[v]?.tip };
    return b ? { ...o, disabled: true, title: b.message, detail: "Can’t run this here" } : o;
  }), st.encoder);
  const rateMode = bitrateMode(st.bitrate_mbps);
  const slider = $("bitrate");
  const b = s.bitrate ?? {};
  slider.min = String(b.min ?? BITRATE.min);
  slider.max = String(b.max ?? BITRATE.max);
  if (document.activeElement !== slider) {
    slider.value = String(b.effective_mbps ?? rateRow(s)?.default_mbps ?? BITRATE.def);
  }
  showBitrate(Number(slider.value), rateMode);
  if (b.table) {
    const rates = b.table.map((r) => `${r.default_mbps} Mb/s at ${PICTURES[r.picture] ?? r.picture}`).join(", ");
    $("bitrate-default").title = `Default: the rate for the picture (${rates})`;
  }
  const advBits = [
    st.codec === "auto" ? null : CODECS.find((c) => c.value === st.codec)?.label,
    st.encoder === "auto" ? null : ENCODERS[st.encoder]?.short,
    rateMode === "manual" ? `${st.bitrate_mbps} Mb/s` : null,
  ].filter(Boolean);
  const advSummary = advBits.length ? advBits.join(" · ") : "Automatic";
  $("adv-summary").textContent = advSummary;
  $("adv-summary").title = advSummary; // the whole text when it is cut with an ellipsis
  $("adv-body").hidden = !advancedOpen;
  $("adv-head").setAttribute("aria-expanded", String(advancedOpen));

  // Audio: on/off, "Also play on PC" (only while audio is on), and the Audio device. They need a sender with --audio.
  renderAudio(s);

  // Keep open (pin)
  renderPin(st.keep_panel_open !== false);

  // Footer
  $("tv-host").textContent = st.tv_host || "Not set";
  $("tv-row").hidden = editingTv;
  $("tv-edit").hidden = !editingTv;
  $("tv-test").disabled = active;
  $("autostart").setAttribute("aria-checked", String(!!s.autostart));
  renderLog(s);
  renderTest(s);
  $("version").textContent = `Version ${s.version}${s.sender ? ` · ${s.sender}` : ""}`;
  requestAnimationFrame(() => {
    placeThumb($("mode"));
    placeThumb($("picture"));
    placeThumb($("codec"));
  });
}

function renderPin(on) {
  $("pin").setAttribute("aria-pressed", String(on));
  $("pin-glyph").textContent = on ? G.pinned : G.pin;
  $("pin-tip").textContent = PIN_TIP[on];
  $("panel").classList.toggle("pinned", on);
  document.body.classList.toggle("pinned", on);
}

function setPrimary(glyph, text) {
  $("primary-glyph").textContent = glyph;
  $("primary-text").textContent = text;
}

function currentMonitor(s) {
  const want = s.settings.mirror_monitor;
  return s.monitors.find((m) => m.device === want) ?? s.monitors.find((m) => m.primary) ?? s.monitors[0];
}

function errorView(s) {
  const e = s.state.error ?? {};
  const host = e.host ?? s.settings.tv_host;
  const retry = { glyph: G.retry, text: "Try again", act: "retry" };
  switch (e.code) {
    case "tv_unreachable":
      return { title: "Can’t reach your TV", sub: `Check that the TV is on and on the same Wi-Fi as this PC (${host}).`,
        primary: retry, secondary: { text: "Change TV", act: "change" } };
    case "tv_app_not_open":
      return s.adb
        ? { title: "TunnelRez isn’t open on the TV", sub: "The TV is on, but its TunnelRez app is closed.",
            primary: { glyph: G.open, text: opening ? "Opening…" : "Open it", act: "open" }, secondary: { text: "Try again", act: "retry" } }
        : { title: "TunnelRez isn’t open on the TV", sub: "The TV is on. Open TunnelRez on it, then try again.",
            primary: retry, secondary: { text: "Change TV", act: "change" } };
    case "sender_missing":
      return { title: "Stream engine missing", sub: "Put tunnelrez-send.exe next to TunnelRez, then try again.", primary: retry };
    case "monitor_mode_failed": {
      const size = sizeWords(e.width, e.height);
      const is4k = size === "4K";
      return { title: `Could not switch the TV monitor to ${size}`,
        sub: monitorSub(s, `Windows did not accept ${size} for the TV screen, so the stream did not start.${is4k ? " Try 1080p60 instead." : ""}`),
        primary: repairOr(retry, s), secondary: is4k ? { text: "Use 1080p60", act: "1080p" } : undefined };
    }
    case "monitor_failed": {
      const mirror = { text: "Use Mirror", act: "mirror" };
      switch (e.cause) {
        case "missing":
          return { title: "The TV screen driver isn’t installed",
            sub: monitorSub(s, s.tv_monitor?.kind === "no_installer"
              ? "TunnelRez adds the TV as a screen with the Parsec display driver. Its installer is not here: get the full kit."
              : "TunnelRez adds the TV as a screen with the Parsec display driver."),
            primary: s.tv_monitor?.action === "install" || s.tv_monitor?.kind === "working"
              ? { glyph: G.retry, text: s.tv_monitor.kind === "working" ? "Installing…" : "Install", act: "vdd_fix" } : retry,
            secondary: mirror };
        case "restart_windows":
          return { title: "Restart Windows to finish", sub: "The TV screen driver is installed, but Windows must restart before it works.",
            primary: retry, secondary: mirror };
        case "disabled":
          return { title: "The TV screen driver is turned off", sub: monitorSub(s, "Repair turns it back on."), primary: repairOr(retry, s), secondary: mirror };
        case "adapter_problem":
          return { title: "The TV screen driver has a problem", sub: monitorSub(s, "Repair restarts it."), primary: repairOr(retry, s), secondary: mirror };
        default:
          return { title: "Couldn’t add the TV screen",
            sub: monitorSub(s, s.tv_monitor?.can_fix ? "Windows did not show it, after a fresh try. Repair restarts the screen driver." : "Windows did not show it, after a fresh try."),
            primary: repairOr(retry, s), secondary: mirror };
      }
    }
    case "unsupported":
      return e.hevc_store
        ? { title: "Windows is missing the HEVC codec",
            sub: "The stream did not start: this encoder needs Windows’ HEVC codec, and it is not installed.", primary: { glyph: G.retry, text: "Use Automatic", act: "auto_encoder" },
            secondary: { text: "Use H.264", act: "h264" } }
        : { title: "This encoder can’t run here", sub: e.message,
            primary: { glyph: G.retry, text: "Use Automatic", act: "auto_encoder" } };
    default:
      return { title: "The stream stopped", sub: e.message || "Something went wrong.", primary: retry };
  }
}

/** The Repair button of a TV monitor error; `fallback` with a sender that cannot repair. */
function repairOr(fallback, s) {
  const t = s.tv_monitor;
  if (!t?.can_fix && t?.kind !== "working") return fallback;
  const text = t.kind === "working" ? "Repairing…" : t.action === "reinstall" ? "Reinstall driver" : "Repair";
  return { glyph: G.retry, text, act: "vdd_fix" };
}

/** A failed repair's own message, shown in place of `sub` on the error screen. */
const monitorSub = (s, sub) => (s.tv_monitor?.kind === "failed" ? s.tv_monitor.message : sub);

/** Where the audio switch goes back to when the user turns it on again: "Also play on PC" off is remembered. */
let audioOnMode = "on";

const AUDIO_TV_HINT = "With a virtual audio device the PC plays a copy; without one, off mutes the PC speakers while streaming";

/**
 * The Audio switch and the "Also play on PC" switch (checked = settings.audio "on", unchecked = "tv_only"). Without
 * `--audio` in the sender they are off, with the reason.
 */
function renderAudio(s) {
  const mode = s.settings.audio ?? "on";
  if (mode !== "off") audioOnMode = mode;
  const sup = s.audio_support ?? { supported: false, tv_only: false, sink: false };
  const on = sup.supported && mode !== "off";
  $("audio").setAttribute("aria-checked", String(on));
  $("audio").disabled = !sup.supported;
  $("audio-state").textContent = !sup.supported ? "Needs a newer sender (this one has no audio)"
    : on ? "Sound plays on the TV" : "Off";
  $("audio-state").title = $("audio-state").textContent;
  // While Audio is off the switch shows what it goes back to.
  const alsoPc = sup.supported && (on ? mode : audioOnMode) === "on";
  $("audio-tv").setAttribute("aria-checked", String(alsoPc));
  $("audio-tv").disabled = !on || !sup.tv_only;
  $("audio-tv-state").textContent = !sup.supported ? "Needs a newer sender"
    : !sup.tv_only ? "This sender can’t keep the PC silent"
    : !on ? "Turn Audio on first"
    : AUDIO_TV_HINT;
  $("audio-tv-state").title = AUDIO_TV_HINT;
  renderAudioDevice(s, sup);
  renderTvSpeaker(s);
  renderTvMonitor(s);
}

/** What the TV monitor row says, by `tv_monitor.kind` ("ready" and "failed" are filled in below). */
const TV_MONITOR_TEXT = {
  unsupported: "Needs a newer sender",
  unknown: "Couldn’t read its status",
  working: "Working…",
  not_installed: "Driver not installed",
  no_installer: "Driver not installed, and its installer was not found",
  restart_required: "Restart Windows to finish",
  disabled: "Driver turned off",
  adapter_problem: "Driver has a problem",
  wrong_gpu_setting: "Set to the wrong graphics chip",
  wrong_gpu: "Running on the wrong graphics chip: slower, with more delay",
  stuck: "A TV screen is stuck",
};

/** The TV monitor row: its status, and Repair or Install when one would help (one Windows prompt at most). */
function renderTvMonitor(s) {
  const t = s.tv_monitor ?? { kind: "unsupported", action: null, can_fix: false };
  const st = $("tv-monitor-state");
  st.textContent = t.kind === "ready" ? "Ready" : t.kind === "failed" ? t.message : TV_MONITOR_TEXT[t.kind] ?? "";
  st.title = t.kind === "ready" && t.version ? `Parsec display driver ${t.version}` : st.textContent;
  st.classList.toggle("hint-line", t.kind === "failed" || t.kind === "wrong_gpu" || t.kind === "no_installer");
  const btn = $("tv-monitor-fix");
  btn.hidden = !(t.action || t.kind === "working");
  btn.disabled = t.kind === "working";
  btn.textContent = { install: "Install", reinstall: "Reinstall driver" }[t.action] ?? "Repair";
}

/** What the TV speaker row says, by `tv_speaker.kind` ("installed" and "failed" are filled in below). */
const TV_SPEAKER_TEXT = {
  unsupported: "Needs a newer sender",
  unknown: "Couldn’t read its status",
  installing: "Installing…",
  not_installed: "Not installed",
  no_driver: "Driver files not found",
  restart_required: "Restart Windows to finish",
  waiting: "Installed, waiting for Windows",
};

/**
 * The TV speaker row: its status, and "Install TV speaker" while it is not installed (the sender shows the Windows
 * prompt; the stream is not restarted, the next one picks the TV speaker up).
 */
function renderTvSpeaker(s) {
  const t = s.tv_speaker ?? { kind: "unsupported", can_install: false, can_check: false };
  const st = $("tv-speaker-state");
  st.textContent = t.kind === "installed" ? "Installed" : t.kind === "failed" ? t.message : TV_SPEAKER_TEXT[t.kind] ?? "";
  st.title = t.kind === "installed" && t.endpoint ? t.endpoint : st.textContent;
  // An error, or the text next to Check again, can be long: it wraps instead of being cut.
  st.classList.toggle("hint-line", t.kind === "failed" || t.kind === "waiting");
  const btn = $("tv-speaker-install");
  btn.hidden = !(t.can_install || t.kind === "installing");
  btn.disabled = t.kind === "installing";
  btn.textContent = t.kind === "failed" ? "Try again" : "Install TV speaker";
  // Windows is still setting up its playback device: read the status again.
  $("tv-speaker-check").hidden = !t.can_check;
}

/** The Audio device picker: Auto (the virtual device the sender would use), Off (the PC speakers), each device. */
function renderAudioDevice(s, sup) {
  const cur = s.settings.audio_sink ?? "";
  const list = s.audio_devices?.devices ?? [];
  const known = !!s.audio_devices;
  const autoName = s.audio_auto_name ?? null;
  const opts = [
    { value: "", label: known ? `Auto (${autoName ?? "none found"})` : "Auto",
      title: "Streams from a virtual audio device when one is installed, else from the PC speakers" },
    { value: "off", label: "Off: capture the PC speakers", title: "Never use a virtual audio device" },
    // The endpoint ID is the value (names need not be unique); the name is the label.
    ...list.map((d) => ({ value: d.id, label: d.name, title: d.adapter || d.name })),
  ];
  // A device saved by name (or an id of a device that is gone): select the listed device it names, else keep it
  // listed, marked.
  const shown = list.find((d) => d.id === cur)?.id ?? list.find((d) => d.name === cur)?.id ?? cur;
  if (shown && shown !== "off" && !list.some((d) => d.id === shown)) {
    opts.push({ value: shown, label: shown, detail: "Not found" });
  }
  audioDevSel.set(opts, shown);
  audioDevSel.btn.disabled = !sup.sink;
  const why = !sup.supported ? "" : !sup.sink ? "Needs a newer sender (this one always captures the PC speakers)" : "";
  $("audio-dev-state").textContent = why;
  $("audio-dev-state").hidden = !why;
}

function renderStats(st) {
  const lat = st?.latency_ms ?? null;
  const cls = latencyClass(lat);
  const heroLat = $("hero-lat");
  heroLat.textContent = fmt(lat);
  heroLat.className = `num ${cls}`;
  $("hero-fps").textContent = fmt(st?.fps);
  const live = $("live");
  const kind = snap?.state.kind;
  live.hidden = !(kind === "streaming" || kind === "connecting");
  $("st-fps").textContent = fmt(st?.fps);
  const l = $("st-lat");
  l.innerHTML = "";
  l.append(fmt(lat));
  if (lat != null) l.insertAdjacentHTML("beforeend", "<small>ms</small>");
  l.className = `num v ${cls}`;
  $("st-lat-k").textContent = st && !st.latency_exact && lat != null ? "Latency (est.)" : "Latency";
  const d = $("st-dec");
  d.innerHTML = "";
  d.append(fmt(st?.decode_ms, 1));
  if (st?.decode_ms != null) d.insertAdjacentHTML("beforeend", "<small>ms</small>");
  renderRate(st);
  // Audio line: from the sender's `audio*` keys; nothing when it sends none (audio off, or an older sender).
  const al = $("audio-line");
  al.hidden = !st?.audio_line;
  al.textContent = st?.audio_line ?? "";
  al.title = st?.audio_line ?? "";
  // Device line: which device the sender captures (`audio_dev`), and whether the PC plays too.
  const dl = $("audio-dev-line");
  dl.hidden = !st?.audio_device_line;
  dl.textContent = st?.audio_device_line ?? "";
  dl.title = st?.audio_device_line ?? "";
  // No virtual device in use although one would be (the setting is not Off): suggest the TV speaker.
  $("audio-sink-hint").hidden = !(st?.audio_route === "loopback" && (snap?.settings.audio_sink ?? "") !== "off");
  const tvs = snap?.tv_speaker;
  $("audio-sink-hint-text").textContent = tvs?.kind === "installed" ? "TV speaker installed. The next stream uses it."
    : tvs?.kind === "installing" ? "Installing the TV speaker…"
    : tvs?.kind === "restart_required" ? "Restart Windows to finish installing the TV speaker."
    : tvs?.kind === "waiting" ? "TV speaker installed. Windows is still setting it up."
    : "Install the TV speaker for best results.";
  $("audio-sink-install").hidden = !tvs?.can_install;
  drawSpark();
}

/** Mb/s tile: what really went out, next to the target; a warning when a fixed rate falls short. */
function renderRate(st) {
  const sent = st?.sent_mbps ?? st?.mbps ?? null;
  const target = st?.target_mbps ?? null;
  const digits = (v) => (v != null && v < 10 ? 1 : 0);
  const m = $("st-mbps");
  m.innerHTML = "";
  m.append(fmt(sent, digits(sent)));
  if (target != null) m.insertAdjacentHTML("beforeend", `<small>/ ${fmt(target, digits(target))}</small>`);
  $("st-mbps-k").textContent = target != null ? "Mb/s sent" : "Mb/s";
  const low = st?.rate === "low";
  $("st-mbps-tile").classList.toggle("low", low);
  $("st-mbps-tile").title = target == null ? ""
    : st.rate === "ceiling" ? `Up to ${fmt(target)} Mb/s (Auto: simple scenes send less)` : `Target ${fmt(target)} Mb/s`;
  const kind = snap?.state.kind;
  $("rate-warn").hidden = !(low && kind === "streaming");
  $("rate-warn-text").textContent = low
    ? `Sending only ${fmt(sent)} of ${fmt(target)} Mb/s. The network or this PC may not keep up; try a lower bitrate.`
    : "";
}

function drawSpark() {
  const c = $("spark");
  if (!c || c.offsetParent === null) return;
  const dpr = window.devicePixelRatio || 1;
  const w = c.clientWidth;
  const h = c.clientHeight;
  c.width = Math.round(w * dpr);
  c.height = Math.round(h * dpr);
  const g = c.getContext("2d");
  g.scale(dpr, dpr);
  g.clearRect(0, 0, w, h);
  const css = getComputedStyle(document.documentElement);
  const pts = history.slice(-60);
  const max = Math.max(40, ...pts) * 1.15;
  const y = (v) => h - 2 - (v / max) * (h - 6);
  // 30 ms guide: the "good" line.
  g.strokeStyle = css.getPropertyValue("--divider");
  g.setLineDash([2, 3]);
  g.lineWidth = 1;
  g.beginPath();
  g.moveTo(0, Math.round(y(30)) + 0.5);
  g.lineTo(w, Math.round(y(30)) + 0.5);
  g.stroke();
  g.setLineDash([]);
  if (pts.length < 2) return;
  const last = pts[pts.length - 1];
  const color = css.getPropertyValue(last < 30 ? "--good" : last < 60 ? "--warn" : "--bad").trim();
  const step = w / 59;
  const x0 = w - (pts.length - 1) * step;
  g.beginPath();
  pts.forEach((v, i) => (i ? g.lineTo(x0 + i * step, y(v)) : g.moveTo(x0, y(v))));
  const grad = g.createLinearGradient(0, 0, 0, h);
  grad.addColorStop(0, `color-mix(in srgb, ${color} 28%, transparent)`);
  grad.addColorStop(1, `color-mix(in srgb, ${color} 0%, transparent)`);
  g.save();
  g.lineTo(w, h);
  g.lineTo(x0, h);
  g.closePath();
  g.fillStyle = grad;
  g.fill();
  g.restore();
  g.beginPath();
  pts.forEach((v, i) => (i ? g.lineTo(x0 + i * step, y(v)) : g.moveTo(x0, y(v))));
  g.strokeStyle = color;
  g.lineWidth = 1.5;
  g.lineJoin = "round";
  g.stroke();
  g.fillStyle = color;
  g.beginPath();
  g.arc(w - 2, y(last), 2.5, 0, Math.PI * 2);
  g.fill();
}

/** `"default"` (not picked: the picture's own rate), `"auto"`, or `"manual"` (a number the user picked). */
function bitrateMode(v) {
  if (v === "auto") return "auto";
  return typeof v === "number" ? "manual" : "default";
}

/** The picked picture's row of the bitrate table: `{ default_mbps, high_mbps, hint }`. */
function rateRow(s) {
  return s?.bitrate?.table?.find((r) => r.picture === s.settings.picture) ?? null;
}

/** The slider at `v` Mb/s in `mode`; also used while it is dragged (then `mode` is "manual"). */
function showBitrate(v, mode) {
  const s = $("bitrate");
  const auto = mode === "auto";
  const min = Number(s.min) || BITRATE.min;
  const max = Number(s.max) || BITRATE.max;
  $("bitrate-value").textContent = auto ? "" : `${v} Mb/s`;
  s.classList.toggle("off", auto);
  s.style.setProperty("--fill", `${((v - min) / (max - min)) * 100}%`);
  s.setAttribute("aria-valuetext", auto ? "Automatic" : `${v} megabits per second${mode === "default" ? ", default" : ""}`);
  $("bitrate-auto").setAttribute("aria-pressed", String(auto));
  $("bitrate-default").setAttribute("aria-pressed", String(mode === "default"));
  // Far above what the picture needs: say that it only adds delay on Wi-Fi.
  const row = rateRow(snap);
  const high = !auto && row?.high_mbps != null && v > row.high_mbps ? row.hint : null;
  $("bitrate-hint").hidden = !high;
  $("bitrate-hint-text").textContent = high ?? "";
}

// ------------------------------------------------------------------------------------------------ logger

/** 102 -> "1:42". */
function clock(sec) {
  return `${Math.floor(sec / 60)}:${String(sec % 60).padStart(2, "0")}`;
}

function renderLog(s) {
  const l = s.log ?? {};
  const on = !!l.active;
  $("log").setAttribute("aria-checked", String(on));
  const state = $("log-state");
  state.classList.toggle("rec", on);
  if (on) state.textContent = `Recording · ${clock(l.remaining_s ?? 0)} left`;
  else if (l.file) state.textContent = `Saved · ${l.seconds} s`;
  else state.textContent = `Off · each second, ${Math.round((l.max_s ?? 120) / 60)} min max`;
  state.title = l.file ?? "";
  logFormatSeg.set(LOG_FORMATS.map((f) => ({ ...f, disabled: on })), s.settings.log_format ?? "markdown");
  $("log-file").hidden = !l.file;
  const rb = l.runbook?.kind ?? "idle";
  const btn = $("log-runbook");
  // One log, one run: once it is in, the button stays off until the next log (the result line says "Added as <id>").
  btn.disabled = !l.runbook_ready;
  btn.textContent = rb === "running" ? "Adding…"
    : rb === "done" ? "Added"
    : "Add to run book";
  btn.title = rb === "done" ? "This log is in the run book"
    : l.runbook_ready ? "Save the last log as a run in docs/runbook (tools/runbook/add-run.ps1)"
    : "Record a log first";
  // What was on screen, for the run book entry: only while there is a log to add.
  const content = $("log-content");
  $("log-content-row").hidden = !(l.runbook_ready || rb === "running");
  content.disabled = rb === "running";
  if (on) content.value = "";
  // A short log, or one where what was on screen changed: say so before it goes into the run book.
  const warn = $("log-warn");
  const warnings = on ? [] : (l.warnings ?? []);
  warn.innerHTML = "";
  if (warnings.length) {
    warn.innerHTML = `<span class="glyph" aria-hidden="true">${G.warn}</span><span class="list"></span>`;
    for (const w of warnings) warn.lastChild.append(Object.assign(document.createElement("span"), { textContent: w }));
  }
  // The test page was on screen the whole log: the run book's content says so unless the user types something.
  if (l.content && !content.value) content.placeholder = l.content;
  else if (!l.content) content.placeholder = "What was on screen (optional)";
  const out = $("log-result");
  const [cls, glyph, text] = l.error ? ["bad", G.bad, l.error]
    : rb === "done" ? ["ok", G.ok, l.runbook.message]
    : rb === "failed" ? ["bad", G.bad, `Run book: ${l.runbook.message}`]
    : [null, null, null];
  out.className = `result log-result${cls ? ` ${cls}` : ""}`;
  out.innerHTML = "";
  if (text) {
    out.innerHTML = `<span class="glyph" aria-hidden="true">${glyph}</span><span></span>`;
    out.lastChild.textContent = text;
  }
}

// ------------------------------------------------------------------------------------------------ test run

/** The Test row: the moving test page full screen on the TV screen, then a log (see `testrun.rs`). */
function renderTest(s) {
  const t = s.test ?? { state: { kind: "off" }, blocked: null };
  const kind = t.state?.kind ?? "off";
  const on = kind === "opening" || kind === "running";
  const btn = $("test");
  btn.textContent = on ? "Stop test" : "Test";
  btn.disabled = !on && !!t.blocked;
  btn.title = on ? "Close the test page and stop its log"
    : s.settings.mode === "mirror" ? "The test page opens only on the TV screen, never on a screen of this PC. Use Extend."
    : t.blocked ?? "Open the moving test page full screen on the TV, then log 2 minutes";
  const state = $("test-state");
  state.classList.toggle("rec", kind === "running");
  state.textContent = kind === "opening" ? "Opening the test page on the TV…"
    : kind === "running" ? "Test page full screen on the TV"
    : t.blocked ?? "Test page + 2 min log";
  state.title = state.textContent;
  const out = $("test-result");
  out.className = "result test-result";
  out.innerHTML = "";
  if (kind === "failed") {
    out.className = "result test-result bad";
    out.innerHTML = `<span class="glyph" aria-hidden="true">${G.bad}</span><span></span>`;
    out.lastChild.textContent = t.state.message;
  }
}

// ------------------------------------------------------------------------------------------------ TV address

const RESULTS = {
  ready: ["ok", G.ok, "TunnelRez on the TV answered."],
  app_not_open: ["warn", G.warn, "The TV is on, but its TunnelRez app is closed."],
  unreachable: ["bad", G.bad, "No answer. Check the address, and that the TV is on."],
  bad_address: ["bad", G.bad, "That address doesn’t look right."],
};

async function testTv(host, out) {
  out.className = "result";
  out.innerHTML = `<span>Testing…</span>`;
  let r;
  try {
    r = await api.invoke("test_tv", { host });
  } catch {
    r = "unreachable";
  }
  const [cls, glyph, text] = RESULTS[r] ?? RESULTS.unreachable;
  out.className = `result ${cls}`;
  out.innerHTML = `<span class="glyph" aria-hidden="true">${glyph}</span><span></span>`;
  out.lastChild.textContent = text;
  if (r === "app_not_open" && snap?.adb) {
    const b = Object.assign(document.createElement("button"), { className: "btn", type: "button", textContent: "Open it" });
    b.addEventListener("click", async () => {
      b.disabled = true;
      b.textContent = "Opening…";
      try {
        await api.invoke("open_tv_app", { host });
        setTimeout(() => testTv(host, out), 2500);
      } catch (e) {
        out.lastChild.previousSibling.textContent = String(e);
      }
    });
    out.append(b);
  }
  return r;
}

function renderSetup(s) {
  const input = $("setup-host");
  if (!input.value && s.settings.tv_host) input.value = s.settings.tv_host;
  $("setup-done").disabled = !input.value.trim();
}

// ------------------------------------------------------------------------------------------------ events

$("primary").addEventListener("click", async () => {
  const kind = snap.state.kind;
  if (kind === "connecting" || kind === "streaming") return api.invoke("stop");
  if (kind === "error") return errorAction(errorView(snap).primary.act);
  return api.invoke("start", { mode: snap.settings.mode });
});
$("secondary").addEventListener("click", () => errorAction(errorView(snap).secondary?.act));

async function errorAction(act) {
  if (act === "retry") return api.invoke("start", { mode: snap.settings.mode });
  if (act === "vdd_fix") return api.invoke("fix_tv_monitor");
  if (act === "change") return openTvEdit();
  if (act === "mirror") {
    await set("mode", "mirror");
    return api.invoke("start", { mode: "mirror" });
  }
  if (act === "auto_encoder") {
    await set("encoder", "auto");
    await set("codec", "auto");
    return api.invoke("start", { mode: snap.settings.mode });
  }
  if (act === "h264") {
    await set("codec", "h264");
    return api.invoke("start", { mode: snap.settings.mode });
  }
  if (act === "1080p") {
    await set("picture", "1080p60");
    return api.invoke("start", { mode: snap.settings.mode });
  }
  if (act === "open") {
    if (opening) return;
    opening = true;
    render(snap);
    try {
      await api.invoke("open_tv_app", { host: snap.settings.tv_host });
      // Give the TV app a moment to start listening.
      await new Promise((r) => setTimeout(r, 2500));
      opening = false;
      return api.invoke("start", { mode: snap.settings.mode });
    } catch (e) {
      opening = false;
      $("subtitle").textContent = String(e);
    }
  }
}

$("store-free").addEventListener("click", () => api.invoke("open_store", { which: "free" }));
$("store-paid").addEventListener("click", () => api.invoke("open_store", { which: "paid" }));

$("adv-head").addEventListener("click", () => {
  advancedOpen = !advancedOpen;
  render(snap);
});
// Moving the slider picks a fixed rate, kept when the picture changes. Default goes back to the picture's own rate
// (it follows 1080p / 4K); the Auto pill switches between Auto and Default.
$("bitrate").addEventListener("input", (e) => showBitrate(Number(e.target.value), "manual"));
$("bitrate").addEventListener("change", (e) => set("bitrate_mbps", Number(e.target.value)));
$("bitrate-default").addEventListener("click", () => set("bitrate_mbps", "default"));
$("bitrate-auto").addEventListener("click", () => {
  const auto = $("bitrate-auto").getAttribute("aria-pressed") === "true";
  set("bitrate_mbps", auto ? "default" : "auto");
});

// Audio: a change while streaming restarts the stream (the app does that, like for codec and bitrate).
$("audio").addEventListener("click", () => {
  const on = $("audio").getAttribute("aria-checked") === "true";
  set("audio", on ? "off" : audioOnMode);
});
$("audio-tv").addEventListener("click", () => {
  const alsoPc = $("audio-tv").getAttribute("aria-checked") === "true";
  set("audio", alsoPc ? "tv_only" : "on");
});
// Install the TV speaker (row and live-card hint): the app runs it off the UI thread and sends snapshots.
const installTvSpeaker = () => api.invoke("install_tv_speaker");
$("tv-speaker-install").addEventListener("click", installTvSpeaker);
$("tv-speaker-check").addEventListener("click", () => api.invoke("refresh_audio"));
$("tv-monitor-fix").addEventListener("click", () => api.invoke("fix_tv_monitor"));
$("audio-sink-install").addEventListener("click", installTvSpeaker);

// Log: a new file each time it is switched on; it stops on its own after 2 minutes.
$("log").addEventListener("click", () => {
  const on = $("log").getAttribute("aria-checked") !== "true";
  $("log").setAttribute("aria-checked", String(on));
  api.invoke("set_logging", { on });
});
$("test").addEventListener("click", () => {
  const kind = snap?.test?.state?.kind;
  api.invoke("set_test", { on: !(kind === "opening" || kind === "running") });
});
const addToRunbook = () => api.invoke("add_to_runbook", { content: $("log-content").value });
$("log-runbook").addEventListener("click", addToRunbook);
$("log-content").addEventListener("keydown", (e) => {
  if (e.key === "Enter" && !$("log-runbook").disabled) addToRunbook();
});
$("log-file").addEventListener("click", () => api.invoke("open_logs"));

function openTvEdit() {
  editingTv = true;
  render(snap);
  const i = $("tv-input");
  i.value = snap.settings.tv_host;
  $("tv-result").innerHTML = "";
  i.focus();
  i.select();
}
function closeTvEdit() {
  editingTv = false;
  render(snap);
  $("tv-change").focus();
}
$("tv-change").addEventListener("click", openTvEdit);
$("tv-test").addEventListener("click", () => testTv($("tv-input").value, $("tv-result")));
$("tv-save").addEventListener("click", async () => {
  await set("tv_host", $("tv-input").value);
  closeTvEdit();
});
$("tv-input").addEventListener("keydown", (e) => {
  if (e.key === "Enter") $("tv-save").click();
  if (e.key === "Escape") {
    e.stopPropagation();
    closeTvEdit();
  }
});

$("autostart").addEventListener("click", async () => {
  const on = $("autostart").getAttribute("aria-checked") !== "true";
  try {
    const now = await api.invoke("set_autostart", { on });
    $("autostart").setAttribute("aria-checked", String(now));
  } catch { /* the snapshot that follows shows the real value */ }
});
$("quit").addEventListener("click", () => api.invoke("quit"));

// Keep open: the pin in the header. On, the panel is a normal window that the user drags by its header.
$("pin").addEventListener("click", async () => {
  const on = $("pin").getAttribute("aria-pressed") !== "true";
  renderPin(on);
  try {
    renderPin(await api.invoke("set_keep_open", { on }));
  } catch { /* the snapshot that follows shows the real value */ }
});
for (const header of [document.querySelector(".hero"), document.querySelector(".setup .brand")]) {
  header.addEventListener("mousedown", (e) => {
    if (e.button !== 0 || MOCK || !$("panel").classList.contains("pinned")) return;
    if (e.target.closest("button, input, a, [role=radio]")) return;
    e.preventDefault();
    TAURI.window.getCurrentWindow().startDragging();
  });
}

$("setup-host").addEventListener("input", () => {
  $("setup-done").disabled = !$("setup-host").value.trim();
  $("setup-result").innerHTML = "";
});
$("setup-host").addEventListener("keydown", (e) => {
  if (e.key === "Enter" && $("setup-host").value.trim()) $("setup-test").click();
});
$("setup-test").addEventListener("click", () => testTv($("setup-host").value, $("setup-result")));
$("setup-done").addEventListener("click", () => api.invoke("finish_setup", { host: $("setup-host").value }));

document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && !popupFor && !editingTv) api.invoke("hide_panel");
});

// Size the window to the content (the app caps it at the screen's height; past that the panel scrolls).
let fitQueued = false;
const fitObserver = new ResizeObserver(() => {
  if (fitQueued || MOCK) return;
  fitQueued = true;
  requestAnimationFrame(() => {
    fitQueued = false;
    api.invoke("fit_panel", { height: $("panel").scrollHeight });
  });
});
fitObserver.observe($("panel"));
// The panel's own box stops growing at the screen height, so also watch what is inside it.
for (const child of $("panel").children) fitObserver.observe(child);

function enter() {
  const p = $("panel");
  p.classList.remove("enter");
  void p.offsetWidth; // restart the animation
  p.classList.add("enter");
  if (!snap || snap.state.kind === "first_run") $("setup-host").focus();
}

// ------------------------------------------------------------------------------------------------ boot

(async function boot() {
  if (MOCK) document.body.classList.add("mock");
  if (MOCK && params.get("tip")) document.body.classList.add("show-tip");
  applyTheme();
  api.listen("snapshot", render);
  api.listen("stats", (st) => {
    if (!snap) return;
    snap.stats = st;
    if (st.tv && st.latency_ms != null) {
      history.push(st.latency_ms);
      if (history.length > 60) history.shift();
    }
    renderStats(st);
    if (snap.state.kind === "streaming") $("subtitle").textContent = streamingSub(snap);
  });
  api.listen("panel-shown", () => {
    enter();
    // The TV speaker or monitor may have changed outside the app: read them again (and the device list).
    api.invoke("refresh_audio");
  });
  api.listen("log", (l) => {
    if (!snap) return;
    snap.log = l;
    renderLog(snap);
    renderTest(snap);
  });
  const first = await api.invoke("get_snapshot");
  advancedOpen = !!first.mockAdvanced;
  render(first);
  // Screenshots: `?open=encoder` opens the Encoder picker.
  if (MOCK && params.get("open") === "encoder") openPopup(encoderSel, () => {});
  if (MOCK && params.get("open") === "audio-dev") openPopup(audioDevSel, () => {});
  if (!MOCK) enter();
  api.invoke("ui_ready");
})();

// ------------------------------------------------------------------------------------------------ mock

function makeMock() {
  const scenario = location.hash.slice(1) || "idle";
  const hist = Array.from({ length: 60 }, (_, i) => 17 + Math.sin(i / 4) * 2.2 + (i % 7 === 0 ? 3 : 0) + (i === 41 ? 9 : 0));
  const base = {
    state: { kind: "idle" },
    settings: { tv_host: "192.0.2.20", mode: "extend", mirror_monitor: null, picture: "1080p60", codec: "auto",
      encoder: "auto", bitrate_mbps: "default", audio: "on", audio_sink: "", sender_path: null, setup_done: true,
      keep_panel_open: new URLSearchParams(location.search).get("pin") !== "0", panel_pos: null, log_format: "markdown",
      schema: 2 },
    stats: null,
    history: [],
    notice: null,
    blocked: {},
    block: null,
    caps_checking: false,
    encoders: ["auto", "nvenc", "nvenc_native", "qsv", "cpu"],
    pictures: ["1080p60", "4k60"],
    monitors: [
      { device: "\\\\.\\DISPLAY1", label: "Built-in display", detail: "2560×1600 · 165 Hz · Main", primary: true },
      { device: "\\\\.\\DISPLAY3", label: "LG ULTRAGEAR", detail: "3840×2160 · 144 Hz", primary: false },
    ],
    sender: "tunnelrez-send.exe",
    autostart: true,
    adb: true,
    accent: null,
    version: "0.3.0",
    bitrate: { effective_mbps: 40, hint: null, min: 10, max: 150, table: [
      { picture: "1080p60", default_mbps: 40, high_mbps: 80, hint: "High bitrate at 1080p adds delay on Wi-Fi with little visible gain" },
      { picture: "4k60", default_mbps: 100, high_mbps: null, hint: null },
    ] },
    log: { active: false, remaining_s: 0, max_s: 120, file: null, seconds: 0, runbook_ready: false,
      runbook: { kind: "idle" }, error: null, warnings: [], content: null },
    test: { state: { kind: "off" }, blocked: "Start the stream first" },
    audio_support: { supported: true, tv_only: true, sink: true },
    audio_devices: { auto: "{0.0.0.00000000}.{steam}", devices: [
      { id: "{0.0.0.00000000}.{speakers}", name: "Speakers (Realtek(R) Audio)", adapter: "Realtek(R) Audio" },
      { id: "{0.0.0.00000000}.{steam}", name: "Speakers (Steam Streaming Speakers)", adapter: "Steam Streaming Speakers" },
    ] },
    audio_auto_name: "Speakers (Steam Streaming Speakers)",
    tv_speaker: { kind: "installed", endpoint: "Speakers (Steam Streaming Speakers)", can_install: false, can_check: false },
    tv_monitor: { kind: "ready", version: "0.45.0.0", action: null, can_fix: true },
  };
  const streaming = { fps: 59.9, latency_ms: 18.4, latency_exact: true, decode_ms: 4.1, mbps: 99.2, sent_mbps: 99.4,
    target_mbps: 100, rate: "fixed", dropped: 0, rtt_ms: 1.9,
    encoder: "hevc/nvenc zero-copy", size: "1920x1080@60", picture: "1080p60", tv: true };
  const S = structuredClone(base);
  const mods = {
    idle: () => {},
    first_run: () => Object.assign(S, { state: { kind: "first_run" }, settings: { ...S.settings, tv_host: "", setup_done: false } }),
    connecting: () => (S.state = { kind: "connecting", mode: "extend", phase: "plugging" }),
    streaming: () => Object.assign(S, { state: { kind: "streaming", mode: "extend" },
      stats: { ...streaming, size: "3840x2160@60", picture: "4K60" }, history: hist,
      settings: { ...S.settings, picture: "4k60" } }),
    streaming_amber: () => Object.assign(S, { state: { kind: "streaming", mode: "extend" },
      stats: { ...streaming, latency_ms: 41.7, decode_ms: 10.5, mbps: 48.2 }, history: hist.map((v) => v + 22) }),
    mirror: () => {
      S.settings.mode = "mirror";
      S.settings.mirror_monitor = "\\\\.\\DISPLAY3";
    },
    unreachable: () => (S.state = { kind: "error", error: { code: "tv_unreachable", host: "192.0.2.20" } }),
    app_not_open: () => (S.state = { kind: "error", error: { code: "tv_app_not_open", host: "192.0.2.20" } }),
    encoder_failed: () => Object.assign(S, { state: { kind: "streaming", mode: "extend" },
      stats: { ...streaming, latency_ms: 27.9, encoder: "hevc/x265 copy", mbps: 14.1 }, history: hist.map((v) => v + 9),
      settings: { ...S.settings, encoder: "nvenc" }, notice: "NVIDIA NVENC did not start, so TunnelRez is using HEVC on the CPU (x265) instead." }),
    advanced: () => {
      S.mockAdvanced = true;
      Object.assign(S.settings, { codec: "hevc", encoder: "nvenc_native", bitrate_mbps: 60 });
      S.bitrate.effective_mbps = 60;
    },
    // The run that showed the overlap: a long summary in the Advanced header (AV1 4K60 native, 150 Mb/s).
    advanced_long: () => {
      S.mockAdvanced = true;
      Object.assign(S.settings, { picture: "4k60", codec: "av1", encoder: "nvenc_native", bitrate_mbps: 150 });
      S.bitrate.effective_mbps = 150;
    },
    // The longest summary the panel can show: longest codec + encoder short labels + a picked rate.
    advanced_longest: () => {
      S.mockAdvanced = true;
      Object.assign(S.settings, { picture: "4k60", codec: "h264", encoder: "qsv", bitrate_mbps: 150 });
      S.bitrate.effective_mbps = 150;
    },
    bitrate_hint: () => {
      S.mockAdvanced = true;
      S.settings.bitrate_mbps = 120;
      S.bitrate.effective_mbps = 120;
    },
    bitrate_4k: () => {
      S.mockAdvanced = true;
      S.settings.picture = "4k60";
      S.bitrate.effective_mbps = 100;
    },
    log_on: () => Object.assign(S, { state: { kind: "streaming", mode: "extend" }, stats: { ...streaming, sent_mbps: 40.1,
      mbps: 40.0, target_mbps: 40 }, history: hist, log: { ...S.log, active: true, remaining_s: 102,
      file: "2026-10-26_21-14-03.md", seconds: 18 } }),
    log_idle_on: () => (S.log = { ...S.log, active: true, remaining_s: 117, file: "2026-10-26_21-20-41.json", seconds: 3 }),
    log_done: () => {
      S.settings.log_format = "json";
      S.log = { ...S.log, file: "2026-10-26_21-14-03.json", seconds: 120, runbook_ready: false,
        runbook: { kind: "done", id: "2026-10-26-04", message: "Added as 2026-10-26-04. Commit docs/runbook to keep it." } };
    },
    // A finished log, not in the run book yet: the content note and the button.
    log_ready: () => {
      S.settings.log_format = "json";
      S.log = { ...S.log, file: "2026-10-26_21-14-03.json", seconds: 120, runbook_ready: true, runbook: { kind: "idle" } };
    },
    rate_low: () => Object.assign(S, { state: { kind: "streaming", mode: "extend" },
      stats: { ...streaming, mbps: 61.0, sent_mbps: 61.2, target_mbps: 150, rate: "low" }, history: hist,
      settings: { ...S.settings, bitrate_mbps: 150 } }),
    auto_rate: () => Object.assign(S, { state: { kind: "streaming", mode: "extend" }, mockAdvanced: true,
      stats: { ...streaming, mbps: 31.0, sent_mbps: 31.2, target_mbps: 100, rate: "ceiling" }, history: hist,
      settings: { ...S.settings, bitrate_mbps: "auto" } }),
    stopping: () => (S.state = { kind: "stopping" }),
    monitor_failed: () => (S.state = { kind: "error", error: { code: "monitor_failed", cause: "not_attached", message: "detail" } }),
    monitor_missing: () => Object.assign(S, { state: { kind: "error", error: { code: "monitor_failed", cause: "missing", message: "detail" } },
      tv_monitor: { kind: "not_installed", action: "install", can_fix: true } }),
    monitor_disabled: () => Object.assign(S, { state: { kind: "error", error: { code: "monitor_failed", cause: "disabled", message: "detail" } },
      tv_monitor: { kind: "disabled", action: "repair", can_fix: true } }),
    monitor_problem: () => Object.assign(S, { state: { kind: "error", error: { code: "monitor_failed", cause: "adapter_problem", message: "detail" } },
      tv_monitor: { kind: "adapter_problem", action: "repair", can_fix: true } }),
    monitor_restart: () => Object.assign(S, { state: { kind: "error", error: { code: "monitor_failed", cause: "restart_windows", message: "detail" } },
      tv_monitor: { kind: "restart_required", action: null, can_fix: true } }),
    // An older sender: no Repair, no words about a driver.
    monitor_old_sender: () => Object.assign(S, { state: { kind: "error", error: { code: "monitor_failed", cause: "unknown", message: "detail" } },
      tv_monitor: { kind: "unsupported", action: null, can_fix: false } }),
    // The TV monitor row in Advanced. Clicking its button in the demo walks through "Working…".
    tv_monitor_wrong_gpu: () => Object.assign(S, { mockAdvanced: true, tv_monitor: { kind: "wrong_gpu", action: "repair", can_fix: true } }),
    tv_monitor_not_installed: () => Object.assign(S, { mockAdvanced: true, tv_monitor: { kind: "not_installed", action: "install", can_fix: true } }),
    tv_monitor_stuck: () => Object.assign(S, { mockAdvanced: true, tv_monitor: { kind: "stuck", action: "repair", can_fix: true } }),
    tv_monitor_failed: () => Object.assign(S, { mockAdvanced: true, tv_monitor: { kind: "failed",
      message: "The administrator prompt was declined. Nothing was changed.", action: "reinstall", can_fix: true } }),
    tv_monitor_working: () => Object.assign(S, { mockAdvanced: true, tv_monitor: { kind: "working", action: null, can_fix: false } }),
    gated: () => {
      S.mockAdvanced = true;
      const msg = "CPU x264 / x265 / SVT-AV1 (FFmpeg) can't encode HEVC at 4K60 on this PC (too slow, 40 fps). Pick NVIDIA NVENC (FFmpeg) or NVIDIA NVENC native or 1080p60.";
      Object.assign(S.settings, { picture: "4k60", codec: "hevc", encoder: "cpu" });
      S.block = { message: msg, hevc_store: false };
      S.blocked = { "cpu|hevc": S.block, "cpu|av1": { message: "CPU x264 / x265 / SVT-AV1 (FFmpeg) can't encode AV1 at 4K60 on this PC (too slow, 48 fps). Pick NVIDIA NVENC (FFmpeg) or NVIDIA NVENC native.", hevc_store: false },
        "qsv|hevc": { message: "Intel Quick Sync (FFmpeg) can't encode HEVC at 4K60 on this PC (too slow, 75 fps). Pick NVIDIA NVENC (FFmpeg) or NVIDIA NVENC native.", hevc_store: false } };
    },
    hevc_missing: () => Object.assign(S, { state: { kind: "error", error: { code: "unsupported", hevc_store: true,
      message: "Windows has no HEVC encoder for this PC's graphics. Install the HEVC Video Extensions, or pick NVENC or the CPU (they don't need it)." } },
      settings: { ...S.settings, codec: "hevc" } }),
    // Audio: the lines in the live card (a virtual device, also on the PC or not; the PC speakers with the hint),
    // the TV with no audio, the device picker's cases, and senders without --audio-sink or --audio.
    audio_on: () => Object.assign(S, { state: { kind: "streaming", mode: "extend" }, history: hist,
      stats: { ...streaming, audio_line: "Audio: on · 45 ms · A/V +12 ms · buf 28 ms",
        audio_device_line: "Audio device: Speakers (Steam Streaming Speakers) · also on PC", audio_route: "sink" } }),
    audio_tv_only: () => Object.assign(S, { state: { kind: "streaming", mode: "extend" }, history: hist,
      settings: { ...S.settings, audio: "tv_only" },
      stats: { ...streaming, audio_line: "Audio: on · TV only · 45 ms · A/V +12 ms · buf 28 ms",
        audio_device_line: "Audio device: Speakers (Steam Streaming Speakers) · PC silent", audio_route: "sink" } }),
    // No virtual device: the sender captures the PC speakers, and the panel suggests installing one.
    audio_loopback: () => Object.assign(S, { state: { kind: "streaming", mode: "extend" }, history: hist,
      audio_devices: { auto: null, devices: [S.audio_devices.devices[0]] }, audio_auto_name: null,
      tv_speaker: { kind: "not_installed", can_install: true, can_check: false },
      stats: { ...streaming, audio_line: "Audio: on · 45 ms · A/V +12 ms · buf 28 ms",
        audio_device_line: "Audio device: Speakers (Realtek(R) Audio) · PC speakers", audio_route: "loopback" } }),
    // Off picked on purpose: the PC speakers, no hint.
    audio_sink_off: () => Object.assign(S, { state: { kind: "streaming", mode: "extend" }, history: hist,
      settings: { ...S.settings, audio_sink: "off" },
      stats: { ...streaming, audio_line: "Audio: on · 45 ms · A/V +12 ms · buf 28 ms",
        audio_device_line: "Audio device: Speakers (Realtek(R) Audio) · PC speakers", audio_route: "loopback" } }),
    // A device picked by name that is not plugged in now.
    audio_sink_missing: () => (S.settings.audio_sink = "Virtual Audio Driver"),
    audio_tv_no_audio: () => Object.assign(S, { state: { kind: "streaming", mode: "extend" }, history: hist,
      stats: { ...streaming, audio_line: "Audio: TV has no audio support" } }),
    audio_off: () => Object.assign(S, { settings: { ...S.settings, audio: "off" } }),
    audio_no_sink_sender: () => Object.assign(S, { audio_support: { supported: true, tv_only: true, sink: false },
      audio_devices: null, audio_auto_name: null, tv_speaker: { kind: "unsupported", can_install: false, can_check: false } }),
    audio_old_sender: () => Object.assign(S, { audio_support: { supported: false, tv_only: false, sink: false },
      audio_devices: null, audio_auto_name: null, tv_speaker: { kind: "unsupported", can_install: false, can_check: false } }),
    // The TV speaker row: not installed (Install offered), installing (the Windows prompt is up), installed, the
    // install failed (Try again), no driver files. Clicking Install in the demo walks through installing.
    tv_speaker_not_installed: () => Object.assign(S, { mockAdvanced: true,
      audio_devices: { auto: null, devices: [S.audio_devices.devices[0]] }, audio_auto_name: null,
      tv_speaker: { kind: "not_installed", can_install: true, can_check: false } }),
    tv_speaker_installing: () => Object.assign(S, { mockAdvanced: true,
      audio_devices: { auto: null, devices: [S.audio_devices.devices[0]] }, audio_auto_name: null,
      tv_speaker: { kind: "installing", can_install: false, can_check: false } }),
    tv_speaker_installed: () => Object.assign(S, { mockAdvanced: true }),
    tv_speaker_failed: () => Object.assign(S, { mockAdvanced: true,
      audio_devices: { auto: null, devices: [S.audio_devices.devices[0]] }, audio_auto_name: null,
      tv_speaker: { kind: "failed", message: "The Windows prompt was declined. Nothing was installed.",
        can_install: true, can_check: false } }),
    tv_speaker_restart: () => Object.assign(S, { mockAdvanced: true,
      audio_devices: { auto: null, devices: [S.audio_devices.devices[0]] }, audio_auto_name: null,
      tv_speaker: { kind: "restart_required", can_install: false, can_check: false } }),
    // Clicking Check again in the demo finds the playback device.
    tv_speaker_waiting: () => Object.assign(S, { mockAdvanced: true,
      audio_devices: { auto: null, devices: [S.audio_devices.devices[0]] }, audio_auto_name: null,
      tv_speaker: { kind: "waiting", can_install: false, can_check: true } }),
    tv_speaker_no_driver: () => Object.assign(S, { mockAdvanced: true,
      audio_devices: { auto: null, devices: [S.audio_devices.devices[0]] }, audio_auto_name: null,
      tv_speaker: { kind: "no_driver", can_install: false, can_check: false } }),
    checking: () => (S.caps_checking = true),
    // The built-in test run: ready while streaming in Extend, opening, running (the log counts down), failed.
    test_ready: () => Object.assign(S, { state: { kind: "streaming", mode: "extend" },
      stats: { ...streaming, size: "3840x2160@60", picture: "4K60", encoder: "av1/native", mbps: 99.8, sent_mbps: 99.8 },
      history: hist, settings: { ...S.settings, picture: "4k60" }, test: { state: { kind: "off" }, blocked: null } }),
    test_opening: () => Object.assign(S, { state: { kind: "streaming", mode: "extend" },
      stats: { ...streaming, size: "3840x2160@60", picture: "4K60", encoder: "av1/native", mbps: 99.8, sent_mbps: 99.8 },
      history: hist, settings: { ...S.settings, picture: "4k60" }, test: { state: { kind: "opening" }, blocked: null } }),
    test_running: () => Object.assign(S, { state: { kind: "streaming", mode: "extend" },
      stats: { ...streaming, size: "3840x2160@60", picture: "4K60", encoder: "av1/native", mbps: 99.8, sent_mbps: 99.8 },
      history: hist, settings: { ...S.settings, picture: "4k60" }, test: { state: { kind: "running" }, blocked: null },
      log: { ...S.log, active: true, remaining_s: 97, file: "2026-10-27_20-14-03.json", seconds: 23 } }),
    test_mirror: () => Object.assign(S, { state: { kind: "streaming", mode: "mirror" }, stats: streaming, history: hist,
      settings: { ...S.settings, mode: "mirror", mirror_monitor: "\\\\.\\DISPLAY3" },
      test: { state: { kind: "off" }, blocked: "Needs Extend (TV screen only)" } }),
    test_failed: () => Object.assign(S, { state: { kind: "streaming", mode: "extend" }, stats: streaming, history: hist,
      test: { state: { kind: "failed", message: "The page did not fill the TV screen in 30 s." }, blocked: null } }),
    // A finished test log, 21 s (run 2026-10-26-13): the warning, and the content filled in for the run book.
    log_short: () => {
      S.settings.log_format = "json";
      S.log = { ...S.log, file: "2026-10-27_20-14-03.json", seconds: 21, runbook_ready: true, runbook: { kind: "idle" },
        warnings: ["Short log: 21 s with the TV connected. A run needs at least 60 s; record it again."],
        content: "motion60 test page" };
    },
    log_changed: () => {
      S.settings.log_format = "json";
      S.log = { ...S.log, file: "2026-10-27_20-20-41.json", seconds: 120, runbook_ready: true, runbook: { kind: "idle" },
        warnings: ["The picture stopped moving for 6 s during the log."], content: null };
    },
    encoder_mf: () => {
      S.mockAdvanced = true;
      S.encoders = ["auto", "nvenc", "nvenc_native", "qsv", "amf", "mf", "cpu"];
      Object.assign(S.settings, { codec: "hevc", encoder: "mf" });
      S.blocked = { "amf|auto": { message: "AMD AMF (FFmpeg) can't encode any codec at 1080p60 on this PC.", hevc_store: false },
        "amf|hevc": { message: "AMD AMF (FFmpeg) can't encode HEVC at 1080p60 on this PC (no AMD GPU/driver). Pick NVIDIA NVENC (FFmpeg) or Intel Quick Sync (FFmpeg) or Windows Media Foundation (FFmpeg).", hevc_store: false },
        "mf|av1": { message: "Windows Media Foundation (FFmpeg) can't encode AV1 at 1080p60 on this PC (it has no encoder for this codec). Pick NVIDIA NVENC (FFmpeg) or CPU x264 / x265 / SVT-AV1 (FFmpeg).", hevc_store: false } };
    },
    mode_failed: () => Object.assign(S, { state: { kind: "error", error: { code: "monitor_mode_failed", width: 3840, height: 2160 } },
      settings: { ...S.settings, picture: "4k60" } }),
  };
  (mods[scenario] ?? mods.idle)();
  return {
    invoke: async (cmd, args) => {
      if (cmd === "get_snapshot") return S;
      if (cmd === "test_tv") return "app_not_open";
      if (cmd === "refresh_audio" && S.tv_speaker.can_check) {
        S.tv_speaker = structuredClone(base.tv_speaker);
        S.audio_devices = structuredClone(base.audio_devices);
        S.audio_auto_name = base.audio_auto_name;
        setTimeout(() => render(S), 300);
      }
      if (cmd === "fix_tv_monitor") {
        S.tv_monitor = { kind: "working", action: null, can_fix: false };
        render(S);
        setTimeout(() => {
          S.tv_monitor = structuredClone(base.tv_monitor);
          render(S);
        }, 1500);
      }
      if (cmd === "install_tv_speaker" && S.tv_speaker.can_install) {
        S.tv_speaker = { kind: "installing", can_install: false, can_check: false };
        render(S);
        setTimeout(() => {
          S.tv_speaker = structuredClone(base.tv_speaker);
          S.audio_devices = structuredClone(base.audio_devices);
          S.audio_auto_name = base.audio_auto_name;
          render(S);
        }, 1500);
      }
      return null;
    },
    listen: () => {},
  };
}
