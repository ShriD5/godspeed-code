// The page side of the `preview_*` agent tools. Injected (idempotently) in
// front of every agent action; `window.__godspeedAgent.run(action)` returns
// a JSON string: {"ok": value} or {"err": message}. The page is untrusted:
// nothing here is a security boundary beyond keeping cookies and storage out
// of eval results while an expression runs.
(() => {
  const VERSION = 1;
  if (window.__godspeedAgent && window.__godspeedAgent.version === VERSION) return;

  // Console capture: installed at document start for agent tabs, and here
  // (late) for tabs the agent adopted.
  if (!window.__godspeedConsole) {
    const buffer = [];
    Object.defineProperty(window, "__godspeedConsole", { value: buffer });
    const format = (value) => {
      if (typeof value === "string") return value;
      if (value instanceof Error) return value.stack || String(value);
      try { return JSON.stringify(value); } catch (_) { return String(value); }
    };
    const push = (level, args) => {
      try {
        buffer.push({ level, text: Array.from(args, format).join(" ").slice(0, 2000), time: Date.now() });
        if (buffer.length > 200) buffer.splice(0, buffer.length - 200);
      } catch (_) {}
    };
    for (const level of ["log", "info", "warn", "error", "debug"]) {
      const original = console[level];
      console[level] = function (...args) { push(level, args); return original.apply(this, args); };
    }
    addEventListener("error", (e) => push("error", [e.message + (e.filename ? ` (${e.filename}:${e.lineno})` : "")]));
    addEventListener("unhandledrejection", (e) => push("error", ["Unhandled rejection: " + format(e.reason)]));
  }

  const SKIP = new Set(["SCRIPT", "STYLE", "NOSCRIPT", "TEMPLATE", "META", "LINK", "HEAD", "SVG", "PATH", "IFRAME"]);
  const LANDMARK = { NAV: "navigation", MAIN: "main", HEADER: "banner", FOOTER: "contentinfo", ASIDE: "complementary", FORM: "form", DIALOG: "dialog", SECTION: "region", UL: "list", OL: "list", TABLE: "table" };
  let refs = new Map();
  let generation = 0;

  const hidden = (el) => {
    if (el.hidden || el.getAttribute("aria-hidden") === "true") return true;
    const style = getComputedStyle(el);
    return style.display === "none" || style.visibility === "hidden" || style.visibility === "collapse";
  };
  const clip = (text, max) => {
    const t = (text || "").replace(/\s+/g, " ").trim();
    return t.length > max ? t.slice(0, max - 1) + "…" : t;
  };
  const role = (el) => {
    const explicit = el.getAttribute("role");
    if (explicit) return explicit.split(" ")[0];
    const tag = el.tagName;
    if (tag === "A" && el.hasAttribute("href")) return "link";
    if (tag === "BUTTON" || tag === "SUMMARY") return "button";
    if (tag === "SELECT") return "combobox";
    if (tag === "TEXTAREA") return "textbox";
    if (tag === "IMG") return "img";
    if (/^H[1-6]$/.test(tag)) return "heading";
    if (tag === "INPUT") {
      const type = (el.getAttribute("type") || "text").toLowerCase();
      if (type === "hidden") return null;
      if (["button", "submit", "reset", "image"].includes(type)) return "button";
      if (type === "checkbox") return "checkbox";
      if (type === "radio") return "radio";
      if (type === "range") return "slider";
      if (type === "search") return "searchbox";
      return "textbox";
    }
    if (el.isContentEditable && (el.parentElement == null || !el.parentElement.isContentEditable)) return "textbox";
    if (tag === "LI") return "listitem";
    if (tag === "LABEL") return "label";
    return LANDMARK[tag] || null;
  };
  const INTERACTIVE = new Set(["link", "button", "combobox", "textbox", "searchbox", "checkbox", "radio", "slider", "switch", "tab", "menuitem", "option", "clickable"]);
  const labelText = (el) => {
    const aria = el.getAttribute("aria-label");
    if (aria) return aria;
    const by = el.getAttribute("aria-labelledby");
    if (by) {
      const text = by.split(/\s+/).map((id) => document.getElementById(id)).filter(Boolean).map((n) => n.textContent).join(" ");
      if (text.trim()) return text;
    }
    if (el.labels && el.labels.length) return Array.from(el.labels, (l) => l.textContent).join(" ");
    return "";
  };
  const name = (el, r) => {
    const label = labelText(el);
    if (label) return clip(label, 120);
    if (el.tagName === "IMG") return clip(el.getAttribute("alt") || "", 120);
    if (el.tagName === "INPUT") {
      const type = (el.getAttribute("type") || "text").toLowerCase();
      if (["button", "submit", "reset"].includes(type)) return clip(el.value || type, 120);
      return clip(el.getAttribute("placeholder") || el.getAttribute("name") || el.getAttribute("title") || "", 120);
    }
    if (el.tagName === "TEXTAREA") return clip(el.getAttribute("placeholder") || el.getAttribute("name") || "", 120);
    if (r && (INTERACTIVE.has(r) || r === "heading")) return clip(el.innerText || el.textContent || el.getAttribute("title") || "", 160);
    return clip(el.getAttribute("title") || "", 120);
  };
  const state = (el, r) => {
    const bits = [];
    if (el.disabled || el.getAttribute("aria-disabled") === "true") bits.push("disabled");
    if (r === "checkbox" || r === "radio" || r === "switch") bits.push(el.checked || el.getAttribute("aria-checked") === "true" ? "checked" : "unchecked");
    if (el.getAttribute("aria-expanded")) bits.push(el.getAttribute("aria-expanded") === "true" ? "expanded" : "collapsed");
    if (el.getAttribute("aria-selected") === "true") bits.push("selected");
    if (r === "heading") bits.push("level=" + (el.tagName.match(/\d/) || [el.getAttribute("aria-level") || "2"])[0]);
    if (r === "textbox" || r === "searchbox" || r === "combobox") {
      const isPassword = el.tagName === "INPUT" && (el.getAttribute("type") || "").toLowerCase() === "password";
      const value = el.tagName === "SELECT" ? (el.selectedOptions[0] ? el.selectedOptions[0].textContent : "") : el.isContentEditable ? el.innerText : el.value;
      if (isPassword) bits.push(value ? "value=••••" : "empty");
      else if (value) bits.push("value=" + JSON.stringify(clip(value, 80)));
    }
    if (r === "link") {
      const href = el.getAttribute("href") || "";
      if (href && !href.startsWith("javascript:")) bits.push("href=" + clip(href, 80));
    }
    return bits;
  };

  const snapshot = (maxNodes, maxBytes) => {
    generation += 1;
    refs = new Map();
    const lines = [];
    let bytes = 0;
    let truncated = false;
    let nodes = 0;
    const emit = (depth, line) => {
      if (truncated) return;
      const text = "  ".repeat(Math.min(depth, 12)) + line;
      if (nodes >= maxNodes || bytes + text.length + 1 > maxBytes) { truncated = true; return; }
      lines.push(text); bytes += text.length + 1; nodes += 1;
    };
    const walk = (node, depth) => {
      if (truncated) return;
      for (const child of node.childNodes) {
        if (truncated) return;
        if (child.nodeType === Node.TEXT_NODE) {
          const text = clip(child.textContent, 200);
          if (text) emit(depth, "text " + JSON.stringify(text));
          continue;
        }
        if (child.nodeType !== Node.ELEMENT_NODE) continue;
        const el = child;
        if (SKIP.has(el.tagName.toUpperCase()) || hidden(el)) continue;
        const clickable = el.hasAttribute("onclick") || (el.hasAttribute("tabindex") && el.tabIndex >= 0);
        const r = role(el) || (clickable ? "clickable" : null);
        let nextDepth = depth;
        if (r) {
          let line = r;
          const n = name(el, r);
          if (n) line += " " + JSON.stringify(n);
          const bits = state(el, r);
          if (bits.length) line += " (" + bits.join(", ") + ")";
          if (INTERACTIVE.has(r) || clickable) {
            const ref = "e" + (refs.size + 1);
            refs.set(ref, new WeakRef(el));
            line += " [ref=" + ref + "]";
          }
          emit(depth, line);
          nextDepth = depth + 1;
          // A control's own text is in its name.
          if (INTERACTIVE.has(r) || r === "heading" || r === "img") continue;
        }
        if (el.shadowRoot) walk(el.shadowRoot, nextDepth);
        walk(el, nextDepth);
      }
    };
    if (document.body) walk(document.body, 0);
    return {
      title: document.title,
      url: location.href,
      loading: document.readyState !== "complete",
      outline: lines.join("\n"),
      refs: refs.size,
      truncated,
      scroll: { x: Math.round(scrollX), y: Math.round(scrollY), height: document.documentElement.scrollHeight, viewport: innerHeight },
    };
  };

  const resolve = (target) => {
    if (target && target.ref) {
      const weak = refs.get(target.ref);
      const el = weak && weak.deref();
      if (!el || !el.isConnected) throw new Error(`ref ${target.ref} is gone (the page changed). Take a new preview_snapshot.`);
      return el;
    }
    if (target && target.selector) {
      let el;
      try { el = document.querySelector(target.selector); } catch (_) { throw new Error(`invalid CSS selector: ${target.selector}`); }
      if (!el) throw new Error(`no element matches ${target.selector}`);
      return el;
    }
    throw new Error("pass a ref or a selector");
  };
  const describe = (el) => {
    const r = role(el) || el.tagName.toLowerCase();
    const n = name(el, r);
    return n ? `${r} ${JSON.stringify(n)}` : r;
  };
  const center = (el) => {
    const rect = el.getBoundingClientRect();
    return { clientX: rect.left + rect.width / 2, clientY: rect.top + rect.height / 2 };
  };

  const click = (target) => {
    const el = resolve(target);
    if (el.disabled) throw new Error(`${describe(el)} is disabled`);
    el.scrollIntoView({ block: "center", inline: "center" });
    const at = center(el);
    const base = { bubbles: true, cancelable: true, composed: true, view: window, button: 0, ...at };
    el.dispatchEvent(new PointerEvent("pointerdown", { ...base, pointerType: "mouse", isPrimary: true }));
    el.dispatchEvent(new MouseEvent("mousedown", base));
    if (typeof el.focus === "function") el.focus({ preventScroll: true });
    el.dispatchEvent(new PointerEvent("pointerup", { ...base, pointerType: "mouse", isPrimary: true }));
    el.dispatchEvent(new MouseEvent("mouseup", base));
    el.click();
    return { clicked: describe(el) };
  };

  const type = (target, text, submit) => {
    const el = resolve(target);
    if (el.disabled || el.readOnly) throw new Error(`${describe(el)} is not editable`);
    el.scrollIntoView({ block: "center" });
    if (typeof el.focus === "function") el.focus({ preventScroll: true });
    if (el.tagName === "INPUT" || el.tagName === "TEXTAREA") {
      const proto = el.tagName === "INPUT" ? HTMLInputElement.prototype : HTMLTextAreaElement.prototype;
      const setter = Object.getOwnPropertyDescriptor(proto, "value").set;
      setter.call(el, text);
      el.dispatchEvent(new InputEvent("input", { bubbles: true, composed: true, inputType: "insertText", data: text }));
      el.dispatchEvent(new Event("change", { bubbles: true }));
    } else if (el.isContentEditable) {
      const selection = getSelection();
      const range = document.createRange();
      range.selectNodeContents(el);
      selection.removeAllRanges();
      selection.addRange(range);
      if (!document.execCommand("insertText", false, text)) el.textContent = text;
    } else {
      throw new Error(`${describe(el)} is not a text field`);
    }
    let submitted = false;
    if (submit) {
      const key = { key: "Enter", code: "Enter", keyCode: 13, which: 13, bubbles: true, cancelable: true, composed: true };
      const proceed = el.dispatchEvent(new KeyboardEvent("keydown", key));
      el.dispatchEvent(new KeyboardEvent("keypress", key));
      el.dispatchEvent(new KeyboardEvent("keyup", key));
      if (proceed && el.form && el.tagName === "INPUT") {
        if (typeof el.form.requestSubmit === "function") el.form.requestSubmit(); else el.form.submit();
        submitted = true;
      }
    }
    return { typed: describe(el), length: text.length, submitted };
  };

  const scroll = (target, dx, dy, to) => {
    const el = target && (target.ref || target.selector) ? resolve(target) : null;
    const scroller = el || document.scrollingElement || document.documentElement;
    // Without smooth scrolling, so the position is final when we read it.
    const saved = scroller.style.scrollBehavior;
    scroller.style.scrollBehavior = "auto";
    if (to === "top") scroller.scrollTop = 0;
    else if (to === "bottom") scroller.scrollTop = scroller.scrollHeight;
    else { scroller.scrollLeft += dx; scroller.scrollTop += dy; }
    scroller.style.scrollBehavior = saved;
    return { x: Math.round(scroller.scrollLeft), y: Math.round(scroller.scrollTop), height: scroller.scrollHeight, viewport: el ? el.clientHeight : innerHeight };
  };

  // Hide cookies and storage while an eval runs, then restore them.
  const guarded = (fn) => {
    const hide = [
      [Document.prototype, "cookie"],
      [window, "localStorage"], [window, "sessionStorage"], [window, "indexedDB"], [window, "caches"],
      [Window.prototype, "localStorage"], [Window.prototype, "sessionStorage"],
    ];
    const saved = [];
    for (const [object, key] of hide) {
      const descriptor = Object.getOwnPropertyDescriptor(object, key);
      if (!descriptor || !descriptor.configurable) continue;
      saved.push([object, key, descriptor]);
      Object.defineProperty(object, key, { configurable: true, get() { throw new Error(`${key} is not available to preview_eval`); }, set() { throw new Error(`${key} is not available to preview_eval`); } });
    }
    try { return fn(); } finally {
      for (const [object, key, descriptor] of saved) Object.defineProperty(object, key, descriptor);
    }
  };
  // The expression arrives compiled into the injected script as `thunk`
  // (no eval, so a page's CSP can't block it).
  const evaluate = (thunk, maxBytes) => {
    if (typeof thunk !== "function") throw new Error("no expression");
    const value = guarded(thunk);
    if (value && typeof value.then === "function") throw new Error("the expression returned a promise; preview_eval is synchronous");
    let json;
    const seen = new WeakSet();
    json = JSON.stringify(value, (key, v) => {
      if (v instanceof Node) return `<${v.nodeName.toLowerCase()}>`;
      if (typeof v === "object" && v !== null) { if (seen.has(v)) return "[circular]"; seen.add(v); }
      if (typeof v === "function") return "[function]";
      if (typeof v === "bigint") return v.toString();
      return v;
    });
    if (json === undefined) return { value: null, type: typeof value };
    if (json.length > maxBytes) return { truncated: true, json: json.slice(0, maxBytes), bytes: json.length };
    return { value: JSON.parse(json) };
  };

  const consoleRead = (max, clear) => {
    const buffer = window.__godspeedConsole || [];
    const entries = buffer.slice(-max).map((e) => ({ level: e.level, text: e.text, ageMs: Date.now() - e.time }));
    const total = buffer.length;
    if (clear) buffer.splice(0, buffer.length);
    return { entries, total };
  };

  const present = (selector, text) => {
    if (selector) {
      let el;
      try { el = document.querySelector(selector); } catch (_) { throw new Error(`invalid CSS selector: ${selector}`); }
      if (!el || hidden(el)) return false;
    }
    if (text && !(document.body && document.body.innerText.includes(text))) return false;
    return true;
  };

  const run = (action, limits, thunk) => {
    switch (action.action) {
      case "snapshot": return snapshot(limits.maxNodes, limits.maxBytes);
      case "click": return click(action);
      case "type": return type(action, action.text, !!action.submit);
      case "scroll": return scroll(action, action.dx || 0, action.dy || 0, action.to);
      case "eval": return evaluate(thunk, limits.maxEvalBytes);
      case "console": return consoleRead(limits.maxConsole, !!action.clear);
      case "wait": return { present: present(action.selector, action.text) };
      case "state": return { title: document.title, url: location.href, readyState: document.readyState };
      default: throw new Error("unknown action " + action.action);
    }
  };

  const agent = {
    version: VERSION,
    run(action, limits, thunk) {
      try {
        return JSON.stringify({ ok: run(action, limits, thunk) });
      } catch (error) {
        return JSON.stringify({ err: String((error && error.message) || error) });
      }
    },
  };
  Object.defineProperty(window, "__godspeedAgent", { value: agent, configurable: true });
})();
