// Optional: node ui/settings.test.cjs. Runs the actual page script without WebView2.
const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");
const script = fs.readFileSync(`${__dirname}/settings.html`, "utf8").match(/<script>([\s\S]*?)<\/script>/)[1];

function page(language) {
  const elements = new Map();
  function element(id) {
    if (!elements.has(id)) {
      let value = "";
      const el = { checked: false, disabled: false, required: false, children: [],
        appendChild(child) { this.children.push(child); },
        reportValidity() { return !this.required || this.value.trim() !== ""; } };
      Object.defineProperty(el, "value", { get: () => value, set: (v) => {
        value = String(v);
        if (id === "language" && !el.children.some((o) => o.value === value)) value = "";
      } });
      elements.set(id, el);
    }
    return elements.get(id);
  }
  const sent = [];
  const window = { __CONFIG__: { idle_minutes: 120, warning_seconds: 60, poll_seconds: 15,
    action: "sleep", autostart: true, language,
    guards: { cpu_above_percent: 20, audio_playing: false, fullscreen: true, processes: [] } },
    __REVISION__: 7, __LANGS__: [{ code: "en", name: "English" }, { code: "ru", name: "Russian" }],
    __I18N__: { number_required: "number required" },
    ipc: { postMessage: (msg) => sent.push(JSON.parse(msg)) } };
  const document = { getElementById: element, querySelectorAll: () => [], createElement: () => ({}) };
  vm.runInNewContext(script, { window, document }, { timeout: 1000 });
  return { element, window, sent, save: () => element("save").onclick() };
}

for (const alias of ["RU", "ru-RU", "ru_RU", "AUTO", "en-US", "unknown"]) {
  const p = page(alias);
  assert.notEqual(p.element("language").value, "");
  p.save();
  assert.equal(p.sent[0].config.language, alias);
  assert.equal(p.sent[0].config.guards.fullscreen, true);
  assert.equal(p.sent[0].revision, 7);
}
const languageChange = page("ru_RU");
languageChange.element("language").value = "en";
languageChange.save();
assert.equal(languageChange.sent[0].config.language, "en");
assert.equal(languageChange.sent[0].config.guards.fullscreen, true);
for (const key of ["idle_minutes", "warning_seconds", "poll_seconds", "cpu_above_percent"]) {
  for (const blank of ["", "   "]) {
    const p = page("auto");
    p.element(key).value = blank;
    p.save();
    assert.equal(p.sent.length, 0, `${key}: blank must not send IPC`);
  }
}
for (const key of ["warning_seconds", "cpu_above_percent"]) {
  const p = page("auto");
  p.element(key).value = "0";
  p.save();
  assert.equal(p.sent.length, 1);
  assert.equal(key === "cpu_above_percent" ? p.sent[0].config.guards[key] : p.sent[0].config[key], 0);
  assert.equal(p.element("save").disabled, true);
  p.window.showResult(true, "applied");
  assert.equal(p.element("save").disabled, false);
}
console.log("Settings actual-script regression checks passed.");
