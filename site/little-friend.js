// Little Friend analytics (littlefriend.io) for the PowDB site. The site key is
// public: it only names the project the tracker reports to. The tracker sets no
// cookies, reads no form values or page text, and strips query strings.
//
// Loads only on the production site (GitHub Pages), so local previews send
// nothing. Goals match event names, so install copies and GitHub links send
// their own named events.
(function () {
  if (location.hostname !== "zvn-dev.github.io") return;

  var SITE_KEY = "lf_G0lPMMRv20gPCelURBHgtV02";

  window.lf =
    window.lf ||
    function () {
      (window.lf.q = window.lf.q || []).push(arguments);
    };

  function load(src, attrs) {
    var script = document.createElement("script");
    script.src = src;
    script.async = false;
    for (var name in attrs) script.setAttribute(name, attrs[name]);
    document.head.appendChild(script);
  }

  load("https://cdn.littlefriend.io/lf.js", { "data-site": SITE_KEY, "data-mode": "journey" });
  load("https://cdn.littlefriend.io/lf-replay.js", { "data-site": SITE_KEY });

  // Install commands are marked with data-install. Only the method is sent,
  // never the copied text.
  document.addEventListener("copy", function () {
    var selection = window.getSelection();
    var node = selection && selection.anchorNode;
    var element = node && (node.nodeType === 1 ? node : node.parentElement);
    var block = element && element.closest("[data-install]");
    if (block) window.lf("track", "install.copy", { method: block.getAttribute("data-install") });
  });

  function onClick(event) {
    if (event.button > 1 || !(event.target instanceof Element)) return;
    var link = event.target.closest("a[href]");
    if (link && link.hostname === "github.com") window.lf("track", "github.click");
  }
  document.addEventListener("click", onClick);
  document.addEventListener("auxclick", onClick);
})();
