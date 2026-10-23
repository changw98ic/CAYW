(function () {
  "use strict";

  var root = document.documentElement;
  var reduceMotion = window.matchMedia && window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  var finePointer = window.matchMedia && window.matchMedia("(hover: hover) and (pointer: fine)").matches;
  root.classList.add("js");

  /* ---------- header: scrolled state ---------- */
  var header = document.querySelector("[data-header]");
  function onScroll() {
    if (header) header.classList.toggle("is-scrolled", window.scrollY > 8);
  }
  onScroll();
  window.addEventListener("scroll", onScroll, { passive: true });

  /* ---------- mobile nav ---------- */
  var navToggle = document.querySelector("[data-nav-toggle]");
  var nav = document.querySelector("[data-nav]");
  function setNav(open) {
    if (!nav || !navToggle) return;
    nav.classList.toggle("is-open", open);
    navToggle.setAttribute("aria-expanded", open ? "true" : "false");
  }
  if (navToggle && nav) {
    navToggle.addEventListener("click", function () {
      setNav(!nav.classList.contains("is-open"));
    });
    nav.querySelectorAll("a").forEach(function (link) {
      link.addEventListener("click", function () { setNav(false); });
    });
    document.addEventListener("keydown", function (e) {
      if (e.key === "Escape") setNav(false);
    });
    document.addEventListener("click", function (e) {
      if (nav.classList.contains("is-open") && !nav.contains(e.target) && !navToggle.contains(e.target)) setNav(false);
    });
  }

  /* ---------- scroll spy ---------- */
  var spyLinks = Array.prototype.slice.call(document.querySelectorAll("[data-spy]"));
  if ("IntersectionObserver" in window && spyLinks.length) {
    var byId = {};
    spyLinks.forEach(function (a) { byId[a.getAttribute("href").slice(1)] = a; });
    var spy = new IntersectionObserver(function (entries) {
      entries.forEach(function (entry) {
        if (!entry.isIntersecting) return;
        spyLinks.forEach(function (a) { a.classList.remove("is-active"); });
        var link = byId[entry.target.id];
        if (link) link.classList.add("is-active");
      });
    }, { rootMargin: "-45% 0px -50% 0px" });
    Object.keys(byId).forEach(function (id) {
      var el = document.getElementById(id);
      if (el) spy.observe(el);
    });
    var top = document.getElementById("top");
    if (top) {
      new IntersectionObserver(function (entries) {
        if (entries[0].isIntersecting) spyLinks.forEach(function (a) { a.classList.remove("is-active"); });
      }, { rootMargin: "-45% 0px -50% 0px" }).observe(top);
    }
  }

  /* ---------- reveal on scroll (with sibling stagger) ---------- */
  var reveals = document.querySelectorAll("[data-reveal]");
  if ("IntersectionObserver" in window && !reduceMotion) {
    var seen = new Map();
    reveals.forEach(function (el) {
      var parent = el.parentElement;
      var idx = seen.get(parent) || 0;
      el.style.setProperty("--rd", Math.min(idx, 5) * 0.07 + "s");
      seen.set(parent, idx + 1);
    });
    var revealer = new IntersectionObserver(function (entries) {
      entries.forEach(function (entry) {
        if (entry.isIntersecting) {
          entry.target.classList.add("is-visible");
          revealer.unobserve(entry.target);
        }
      });
    }, { rootMargin: "0px 0px -8% 0px", threshold: 0.08 });
    reveals.forEach(function (el) { revealer.observe(el); });
  } else {
    reveals.forEach(function (el) { el.classList.add("is-visible"); });
  }

  /* ---------- hero runtime log: replay rows ---------- */
  var log = document.querySelector("[data-log]");
  if (log && !reduceMotion) {
    var replay = function () {
      log.classList.remove("is-running");
      void log.offsetWidth; // restart CSS animations
      log.classList.add("is-running");
    };
    replay();
    var timer = setInterval(function () {
      if (document.visibilityState === "visible") replay();
    }, 9000);
    window.addEventListener("pagehide", function () { clearInterval(timer); });
  }

  /* ---------- spotlight cards (pointer-follow glow) ---------- */
  if (finePointer) {
    document.querySelectorAll("[data-spotlight]").forEach(function (card) {
      card.addEventListener("pointermove", function (e) {
        var r = card.getBoundingClientRect();
        card.style.setProperty("--mx", e.clientX - r.left + "px");
        card.style.setProperty("--my", e.clientY - r.top + "px");
      });
    });
  }

  /* ---------- subtle tilt on hero console ---------- */
  var tilt = document.querySelector("[data-tilt]");
  if (tilt && finePointer && !reduceMotion) {
    var area = tilt.parentElement;
    var raf = 0;
    area.addEventListener("pointermove", function (e) {
      if (raf) return;
      raf = requestAnimationFrame(function () {
        raf = 0;
        var r = area.getBoundingClientRect();
        var x = (e.clientX - r.left) / r.width - 0.5;
        var y = (e.clientY - r.top) / r.height - 0.5;
        tilt.style.setProperty("--ry", (x * 5).toFixed(2) + "deg");
        tilt.style.setProperty("--rx", (-y * 4).toFixed(2) + "deg");
      });
    });
    area.addEventListener("pointerleave", function () {
      tilt.style.setProperty("--ry", "0deg");
      tilt.style.setProperty("--rx", "0deg");
    });
  }

  /* ---------- copy buttons ---------- */
  document.querySelectorAll("[data-copy]").forEach(function (btn) {
    var label = btn.querySelector("span") || btn;
    var original = label.textContent;
    btn.addEventListener("click", function () {
      var target = document.getElementById(btn.getAttribute("data-copy"));
      if (!target) return;
      var text;
      if (target.tagName === "PRE" || target.tagName === "CODE") {
        text = target.textContent || "";
      } else {
        text = Array.prototype.map.call(target.querySelectorAll("p"), function (p) {
          return p.textContent.replace(/\s+/g, " ").trim();
        }).join("\n");
      }
      var done = function () {
        label.textContent = "已复制";
        btn.classList.add("is-copied");
        clearTimeout(btn._t);
        btn._t = setTimeout(function () {
          label.textContent = original;
          btn.classList.remove("is-copied");
        }, 1600);
      };
      if (navigator.clipboard && navigator.clipboard.writeText) {
        navigator.clipboard.writeText(text).then(done).catch(function () { fallbackCopy(text, done); });
      } else {
        fallbackCopy(text, done);
      }
    });
  });

  function fallbackCopy(text, done) {
    var ta = document.createElement("textarea");
    ta.value = text;
    ta.setAttribute("readonly", "");
    ta.style.position = "absolute";
    ta.style.left = "-9999px";
    document.body.appendChild(ta);
    ta.select();
    try {
      document.execCommand("copy");
      done();
    } catch (e) {
      /* ignore */
    }
    document.body.removeChild(ta);
  }
})();
