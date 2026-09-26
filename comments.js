// Giscus comments for the mini-harness tutorial.
//
// Every page maps to its own GitHub Discussion (mapping = pathname), so a
// question on "第 7 章" stays attached to that chapter. The four IDs below
// come from https://giscus.app for the nothiny/mini-harness repository.
//
// Prerequisite: the giscus GitHub App must be installed on the repository
// (https://github.com/apps/giscus). Without it the widget renders an
// install prompt instead of the comment box.
(function () {
  "use strict";

  var CONFIG = {
    repo: "nothiny/mini-harness",
    repoId: "R_kgDOUq-kYA",
    category: "Announcements",
    categoryId: "DIC_kwDOUq-kYM4DGcIk",
    mapping: "pathname",
    lang: "zh-CN"
  };

  // mdBook sets the active theme as a class on <html> ("light", "rust",
  // "coal", "navy", "ayu"). Map it onto a giscus theme.
  function giscusTheme() {
    var cls = document.documentElement.className || "";
    return /\b(light|rust)\b/.test(cls) ? "light" : "dark";
  }

  function syncTheme() {
    var frame = document.querySelector("iframe.giscus-frame");
    if (!frame || !frame.contentWindow) return;
    frame.contentWindow.postMessage(
      { giscus: { setConfig: { theme: giscusTheme() } } },
      "https://giscus.app"
    );
  }

  function mount() {
    var main = document.querySelector("main");
    if (!main || document.getElementById("giscus-thread")) return;

    var section = document.createElement("section");
    section.id = "giscus-thread";
    section.className = "giscus-thread";

    var heading = document.createElement("h2");
    heading.textContent = "评论";

    var container = document.createElement("div");
    container.className = "giscus";

    section.appendChild(heading);
    section.appendChild(container);
    main.appendChild(section);

    var script = document.createElement("script");
    script.src = "https://giscus.app/client.js";
    script.async = true;
    script.crossOrigin = "anonymous";
    var attrs = {
      "data-repo": CONFIG.repo,
      "data-repo-id": CONFIG.repoId,
      "data-category": CONFIG.category,
      "data-category-id": CONFIG.categoryId,
      "data-mapping": CONFIG.mapping,
      "data-strict": "0",
      "data-reactions-enabled": "1",
      "data-emit-metadata": "0",
      "data-input-position": "top",
      "data-theme": giscusTheme(),
      "data-lang": CONFIG.lang,
      "data-loading": "lazy"
    };
    Object.keys(attrs).forEach(function (key) {
      script.setAttribute(key, attrs[key]);
    });
    container.appendChild(script);

    // Keep giscus in sync when the reader switches mdBook themes.
    new MutationObserver(syncTheme).observe(document.documentElement, {
      attributes: true,
      attributeFilter: ["class"]
    });
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", mount);
  } else {
    mount();
  }
})();
