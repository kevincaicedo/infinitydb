/* InfinityDB website behaviour. No dependencies; every feature degrades to
   readable static HTML without JavaScript. */
(function () {
  "use strict";
  var root = document.documentElement;
  var mq = window.matchMedia ? window.matchMedia("(prefers-color-scheme: dark)") : null;
  var still = window.matchMedia ? window.matchMedia("(prefers-reduced-motion: reduce)").matches : false;
  var $$ = function (sel, el) { return Array.prototype.slice.call((el || document).querySelectorAll(sel)); };

  /* ---------- theme ---------- */
  function effectiveTheme() {
    var t = root.getAttribute("data-theme");
    if (t === "light" || t === "dark") return t;
    return mq && mq.matches ? "dark" : "light";
  }
  function syncTheme() {
    var t = effectiveTheme();
    $$("[data-theme-set]").forEach(function (b) { b.setAttribute("aria-pressed", String(b.getAttribute("data-theme-set") === t)); });
    document.dispatchEvent(new CustomEvent("inf-theme"));
  }
  function setTheme(t) {
    root.setAttribute("data-theme", t);
    try { localStorage.setItem("inf-theme", t); } catch (e) { /* storage unavailable: theme lasts this page */ }
    syncTheme();
  }
  $$("[data-theme-toggle]").forEach(function (b) {
    b.addEventListener("click", function () { setTheme(effectiveTheme() === "dark" ? "light" : "dark"); });
  });
  $$("[data-theme-set]").forEach(function (b) {
    b.addEventListener("click", function () { setTheme(b.getAttribute("data-theme-set")); });
  });
  if (mq && mq.addEventListener) mq.addEventListener("change", syncTheme);
  syncTheme();

  /* ---------- site menu (phones and tablets) ---------- */
  var menu = document.getElementById("menu");
  var menuOpener = null;
  function openMenu(btn) {
    if (!menu) return;
    menuOpener = btn;
    menu.hidden = false;
    document.body.classList.add("locked");
    $$("[data-menu-open]").forEach(function (b) { b.setAttribute("aria-expanded", "true"); });
    var close = menu.querySelector("[data-menu-close]");
    if (close) close.focus();
  }
  function closeMenu() {
    if (!menu || menu.hidden) return;
    menu.hidden = true;
    document.body.classList.remove("locked");
    $$("[data-menu-open]").forEach(function (b) { b.setAttribute("aria-expanded", "false"); });
    if (menuOpener) menuOpener.focus();
  }
  $$("[data-menu-open]").forEach(function (b) { b.addEventListener("click", function () { openMenu(b); }); });
  $$("[data-menu-close]").forEach(function (b) { b.addEventListener("click", closeMenu); });
  if (menu) $$("a", menu).forEach(function (a) { a.addEventListener("click", closeMenu); });

  /* ---------- docs navigation drawer ---------- */
  var docnav = document.getElementById("docnav");
  var scrim = document.querySelector(".scrim");
  var drawerOpener = null;
  function openDrawer(btn) {
    if (!docnav) return;
    drawerOpener = btn;
    docnav.classList.add("open");
    if (scrim) scrim.hidden = false;
    document.body.classList.add("locked");
    $$("[data-docnav-open][aria-controls]").forEach(function (b) { b.setAttribute("aria-expanded", "true"); });
    var target = btn && btn.hasAttribute("data-focus-search") ? docnav.querySelector("[data-search-input]") : docnav.querySelector("[data-docnav-close]");
    if (target) setTimeout(function () { target.focus(); }, 30);
  }
  function closeDrawer() {
    if (!docnav || !docnav.classList.contains("open")) return;
    docnav.classList.remove("open");
    if (scrim) scrim.hidden = true;
    document.body.classList.remove("locked");
    $$("[data-docnav-open][aria-controls]").forEach(function (b) { b.setAttribute("aria-expanded", "false"); });
    if (drawerOpener) drawerOpener.focus();
  }
  $$("[data-docnav-open]").forEach(function (b) { b.addEventListener("click", function () { openDrawer(b); }); });
  $$("[data-docnav-close]").forEach(function (b) { b.addEventListener("click", closeDrawer); });
  if (docnav) $$("a", docnav).forEach(function (a) { a.addEventListener("click", closeDrawer); });

  document.addEventListener("keydown", function (e) {
    if (e.key === "Escape") { closeMenu(); closeDrawer(); }
    if ((e.metaKey || e.ctrlKey) && (e.key === "k" || e.key === "K")) {
      var desk = document.getElementById("doc-search");
      if (!desk) return;
      e.preventDefault();
      if (desk.offsetParent !== null) desk.focus();
      else openDrawer({ hasAttribute: function () { return true; }, focus: function () {} });
    }
  });

  /* ---------- copy ---------- */
  function copyText(text, done) {
    var ok = function () { if (done) done(); };
    if (navigator.clipboard && window.isSecureContext) {
      navigator.clipboard.writeText(text).then(ok, function () { fallback(); });
    } else fallback();
    function fallback() {
      var ta = document.createElement("textarea");
      ta.value = text; ta.setAttribute("readonly", ""); ta.style.position = "fixed"; ta.style.opacity = "0";
      document.body.appendChild(ta); ta.select();
      try { document.execCommand("copy"); ok(); } catch (e) { /* nothing to report: the text stays selectable */ }
      document.body.removeChild(ta);
    }
  }
  var CHECK = '<svg width="14" height="14" viewBox="0 0 14 14" fill="none" aria-hidden="true"><path d="M2.5 7.5l3 3 6-7" stroke="currentColor" stroke-width="1.5"/></svg>';
  $$("[data-copy]").forEach(function (btn) {
    btn.addEventListener("click", function () {
      var block = btn.closest(".codeblock");
      var lines = $$(".ln-cmd", block);
      if (!lines.length) lines = $$(".ln", block);
      var text = lines.map(function (l) {
        var c = l.cloneNode(true);
        $$(".pr", c).forEach(function (p) { p.remove(); });
        return c.textContent.replace(/^ /, "");
      }).join("\n");
      var icon = btn.innerHTML, label = btn.getAttribute("aria-label");
      copyText(text, function () {
        btn.innerHTML = CHECK; btn.classList.add("done"); btn.setAttribute("aria-label", "Copied");
        setTimeout(function () { btn.innerHTML = icon; btn.classList.remove("done"); btn.setAttribute("aria-label", label); }, 1400);
      });
    });
  });
  $$("[data-copy-target]").forEach(function (btn) {
    btn.addEventListener("click", function () {
      var el = document.querySelector(btn.getAttribute("data-copy-target"));
      if (!el) return;
      var t = btn.textContent;
      copyText(el.value, function () { btn.textContent = "Copied"; setTimeout(function () { btn.textContent = t; }, 1400); });
    });
  });
  $$("[data-copy-url]").forEach(function (btn) {
    btn.addEventListener("click", function () {
      var span = btn.querySelector("span"), t = span ? span.textContent : "";
      copyText(location.href.split("#")[0], function () { if (span) { span.textContent = "Copied"; setTimeout(function () { span.textContent = t; }, 1400); } });
    });
  });
  $$("[data-feed-url]").forEach(function (el) {
    if (/^https?:$/.test(location.protocol)) el.value = new URL("feed.xml", location.href).href;
  });

  /* ---------- filters (blog topics, compat statuses) ---------- */
  $$("[data-filter-group]").forEach(function (group) {
    var target = document.querySelector(group.getAttribute("data-filter-group"));
    if (!target) return;
    var buttons = $$("[data-filter]", group);
    buttons.forEach(function (b) {
      b.addEventListener("click", function () {
        var f = b.getAttribute("data-filter");
        buttons.forEach(function (x) {
          var on = x === b;
          x.setAttribute("aria-pressed", String(on));
          x.classList.toggle("chip-on", on);
        });
        $$("[data-group]", target).forEach(function (row) {
          row.hidden = !(f === "all" || row.getAttribute("data-group") === f);
        });
      });
    });
  });

  /* ---------- docs search ---------- */
  function searchIndex() { return window.INF_SEARCH || []; }
  function norm(s) { return s.toLowerCase().replace(/&/g, "and"); }
  function find(q) {
    var words = norm(q).split(/\s+/).filter(Boolean);
    if (!words.length) return [];
    var out = [];
    searchIndex().forEach(function (p) {
      var hay = norm(p.t + " " + p.s);
      var score = 0;
      if (words.every(function (w) { return hay.indexOf(w) >= 0; })) {
        score = norm(p.t).indexOf(words[0]) === 0 ? 3 : 2;
        out.push({ score: score, title: p.t, section: p.s, url: p.u });
      }
      p.h.forEach(function (h) {
        var text = norm(h[1] + " " + p.t);
        if (words.every(function (w) { return text.indexOf(w) >= 0; })) {
          out.push({ score: 1, title: h[1], section: p.t, url: p.u + "#" + h[0] });
        }
      });
    });
    out.sort(function (a, b) { return b.score - a.score; });
    return out.slice(0, 8);
  }
  $$("input[aria-controls]").forEach(function (input) {
    var list = document.getElementById(input.getAttribute("aria-controls"));
    if (!list) return;
    var active = -1, results = [];
    function render() {
      list.innerHTML = "";
      if (!input.value.trim()) { list.hidden = true; input.setAttribute("aria-expanded", "false"); return; }
      results = find(input.value);
      if (!results.length) {
        var none = document.createElement("li");
        none.className = "none"; none.textContent = "No match in the docs.";
        list.appendChild(none);
      }
      results.forEach(function (r, i) {
        var li = document.createElement("li");
        li.setAttribute("role", "option");
        li.id = list.id + "-" + i;
        li.setAttribute("aria-selected", String(i === active));
        var a = document.createElement("a");
        a.href = r.url;
        a.textContent = r.title;
        var s = document.createElement("span");
        s.textContent = r.section;
        a.appendChild(s);
        li.appendChild(a);
        list.appendChild(li);
      });
      list.hidden = false;
      input.setAttribute("aria-expanded", "true");
      if (active >= 0) input.setAttribute("aria-activedescendant", list.id + "-" + active);
      else input.removeAttribute("aria-activedescendant");
    }
    input.addEventListener("input", function () { active = -1; render(); });
    input.addEventListener("keydown", function (e) {
      if (e.key === "ArrowDown") { e.preventDefault(); active = Math.min(results.length - 1, active + 1); render(); }
      else if (e.key === "ArrowUp") { e.preventDefault(); active = Math.max(-1, active - 1); render(); }
      else if (e.key === "Enter") {
        var r = results[active >= 0 ? active : 0];
        if (r) { e.preventDefault(); location.href = r.url; }
      } else if (e.key === "Escape") { input.value = ""; render(); }
    });
    input.addEventListener("blur", function () { setTimeout(function () { list.hidden = true; input.setAttribute("aria-expanded", "false"); }, 150); });
    input.addEventListener("focus", render);
  });

  /* ---------- on-this-page tracking ---------- */
  var tocLinks = $$("[data-toc]");
  if (tocLinks.length) {
    var heads = tocLinks.map(function (a) { return document.getElementById(a.getAttribute("data-toc")); });
    var ticking = false;
    var update = function () {
      ticking = false;
      var current = 0;
      heads.forEach(function (h, i) { if (h && h.getBoundingClientRect().top < 140) current = i; });
      tocLinks.forEach(function (a, i) { a.classList.toggle("on", i === current); });
    };
    window.addEventListener("scroll", function () { if (!ticking) { ticking = true; requestAnimationFrame(update); } }, { passive: true });
    update();
  }

  /* ---------- figure scale, where CSS cannot compute it ---------- */
  var cssScale = window.CSS && CSS.supports && CSS.supports("transform", "scale(tan(atan2(100cqw, 558px)))");
  if (!cssScale && window.ResizeObserver) {
    var ro = new ResizeObserver(function (entries) {
      entries.forEach(function (en) {
        var fig = en.target, stage = fig.querySelector(".fig-stage");
        var base = fig.classList.contains("fig-wide") ? 1198 : 558;
        if (stage) stage.style.setProperty("--k", String(fig.clientWidth / base));
      });
    });
    $$(".fig").forEach(function (f) { ro.observe(f); });
  }

  /* ---------- the pixel field ----------
     1-bit, 8 px cells and 4 px pixels (6 px cells on phones), soft edges
     from a 4x4 ordered dither, motion a pure function of the frame
     number. One signal pixel rides the crest. */
  var BAYER = [0, 8, 2, 10, 12, 4, 14, 6, 3, 11, 1, 9, 15, 7, 13, 5];
  function smooth(a, b, x) { var t = Math.min(1, Math.max(0, (x - a) / (b - a))); return t * t * (3 - 2 * t); }
  function token(name) { return getComputedStyle(root).getPropertyValue(name).trim(); }

  function drawWide(ctx, W, H, oy, frame, col) {
    var CELL = 8, SIZE = 4, t = frame / 30;
    var cols = Math.floor(W / CELL), rows = Math.floor(H / CELL);
    ctx.fillStyle = col.ink;
    for (var c = 0; c < cols; c++) {
      var u = c / cols;
      var env = smooth(0.26, 0.78, u);
      if (env < 0.01) continue;
      var base = H - H * 0.5 * u;
      var y1 = base + 40 * Math.sin(c * 0.045 + t * 0.7) + 18 * Math.sin(c * 0.017 - t * 0.4 + 1.3);
      var s1 = 20 + 9 * Math.sin(c * 0.03 + t * 0.5);
      var y2 = y1 + 62 + 16 * Math.sin(c * 0.06 - t * 0.9 + 2.1);
      var y3 = y1 - 104 + 26 * Math.sin(c * 0.028 + t * 0.55 + 4.0);
      var top = Math.max(0, Math.floor((y3 - 50) / CELL));
      var bottom = Math.min(rows - 1, Math.ceil((y2 + 30) / CELL));
      for (var r = top; r <= bottom; r++) {
        var y = r * CELL + CELL / 2;
        var d1 = (y - y1) / s1, d2 = (y - y2) / 10, d3 = (y - y3) / 16;
        var v = Math.exp(-d1 * d1);
        var v2 = 0.72 * Math.exp(-d2 * d2); if (v2 > v) v = v2;
        var v3 = 0.34 * Math.exp(-d3 * d3); if (v3 > v) v = v3;
        v *= env;
        if (v > (BAYER[(c & 3) + ((r & 3) << 2)] + 0.5) / 16) ctx.fillRect(c * CELL + 2, oy + r * CELL + 2, SIZE, SIZE);
      }
    }
    var span = cols + 60;
    var packet = Math.floor(((frame * 0.7) % span) - 30);
    if (packet >= 0 && packet < cols) {
      var pu = packet / cols;
      if (smooth(0.26, 0.78, pu) > 0.35) {
        var pb = H - H * 0.5 * pu;
        var py = pb + 40 * Math.sin(packet * 0.045 + t * 0.7) + 18 * Math.sin(packet * 0.017 - t * 0.4 + 1.3);
        ctx.fillStyle = col.signal;
        ctx.fillRect(packet * CELL + 1, oy + Math.round((py - CELL / 2) / CELL) * CELL + 1, 6, 6);
      }
    }
  }

  function drawPhone(ctx, W, H, oy, frame, col) {
    var CELL = 6, t = frame / 30;
    var cols = Math.floor(W / CELL), rows = Math.floor(H / CELL);
    ctx.fillStyle = col.ink;
    for (var c = 0; c < cols; c++) {
      var u = c / cols;
      var env = 0.35 + 0.65 * smooth(0, 0.6, u);
      var y1 = H * 0.62 - H * 0.22 * u + 22 * Math.sin(c * 0.09 + t * 0.7) + 10 * Math.sin(c * 0.035 - t * 0.4 + 1.3);
      var s1 = 13 + 5 * Math.sin(c * 0.06 + t * 0.5);
      var y2 = y1 + 38 + 9 * Math.sin(c * 0.12 - t * 0.9 + 2.1);
      var y3 = y1 - 58 + 14 * Math.sin(c * 0.055 + t * 0.55 + 4.0);
      for (var r = 0; r < rows; r++) {
        var y = r * CELL + CELL / 2;
        var d1 = (y - y1) / s1, d2 = (y - y2) / 7, d3 = (y - y3) / 10;
        var v = Math.exp(-d1 * d1);
        var v2 = 0.72 * Math.exp(-d2 * d2); if (v2 > v) v = v2;
        var v3 = 0.34 * Math.exp(-d3 * d3); if (v3 > v) v = v3;
        v *= env * smooth(0, 36, y);
        if (v > (BAYER[(c & 3) + ((r & 3) << 2)] + 0.5) / 16) ctx.fillRect(c * CELL + 1, oy + r * CELL + 1, 4, 4);
      }
    }
    var span = cols + 30;
    var packet = Math.floor(((frame * 0.45) % span) - 15);
    if (packet >= 0 && packet < cols) {
      var pu = packet / cols;
      var py = H * 0.62 - H * 0.22 * pu + 22 * Math.sin(packet * 0.09 + t * 0.7) + 10 * Math.sin(packet * 0.035 - t * 0.4 + 1.3);
      ctx.fillStyle = col.signal;
      ctx.fillRect(packet * CELL, oy + Math.round((py - CELL / 2) / CELL) * CELL, 6, 6);
    }
  }

  $$("canvas[data-field]").forEach(function (el) {
    var ctx = el.getContext && el.getContext("2d");
    if (!ctx) return;
    var frame = still ? 240 : 0, last = 0, visible = true, col = null;
    function colors() { col = { ink: token("--ink"), signal: token("--signal"), muted: token("--muted") }; }
    function draw() {
      var W = el.clientWidth, H = el.clientHeight;
      if (!W || !H) return;
      var dpr = Math.min(2, window.devicePixelRatio || 1);
      if (el.width !== Math.round(W * dpr) || el.height !== Math.round(H * dpr)) {
        el.width = Math.round(W * dpr); el.height = Math.round(H * dpr);
      }
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      ctx.clearRect(0, 0, W, H);
      if (!col) colors();
      if (W < 640) {
        var ph = Math.min(H, 260);
        drawPhone(ctx, W, ph, H - ph, still ? 200 : frame, col);
      } else {
        var wh = W >= 900 ? H : Math.min(H, 380);
        drawWide(ctx, W, wh, H - wh, frame, col);
        if (W >= 900) {
          ctx.fillStyle = col.muted;
          ctx.font = '11px "Geist Mono", ui-monospace, monospace';
          ctx.textAlign = "right";
          ctx.fillText("fig. 00 — pixel field · deterministic · frame " + String(frame).padStart(6, "0"), W - 24, H - 22);
        }
      }
    }
    document.addEventListener("inf-theme", function () { colors(); draw(); });
    window.addEventListener("resize", draw);
    if (document.fonts && document.fonts.ready) document.fonts.ready.then(draw);
    if (window.IntersectionObserver) {
      new IntersectionObserver(function (en) { visible = en[0].isIntersecting; }).observe(el);
    }
    draw();
    if (still) return;
    (function loop(ts) {
      requestAnimationFrame(loop);
      if (!visible || document.hidden || ts - last < 33) return;
      last = ts;
      draw();
      frame += 1;
    })(0);
  });
})();
