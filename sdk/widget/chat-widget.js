/**
 * Little Monkey embeddable chat widget.
 *
 * Self-contained: no external CDN or script dependency, no external
 * stylesheet or font, no analytics. Talks only to the local API server base
 * URL you configure it with -- POSTs to its existing `/chat/completions`
 * route using a scoped bearer token. It never touches the filesystem or any
 * agent tool: the local API server itself already refuses to expose that
 * surface (see `src-tauri/src/server.rs`'s module doc comment).
 *
 * Usage: set `window.LMK_CHAT_WIDGET_CONFIG` before this script runs, e.g.
 *
 *   <script>
 *     window.LMK_CHAT_WIDGET_CONFIG = {
 *       baseUrl: "http://127.0.0.1:1234/v1",
 *       token: "lmk-...",         // scope this token to `chat` only
 *       model: "qwen2.5-7b-instruct",
 *       title: "Ask the homelab",
 *     };
 *   </script>
 *   <script src="./chat-widget.js"></script>
 */
(function () {
  "use strict";

  var config = window.LMK_CHAT_WIDGET_CONFIG || {};
  if (!config.baseUrl) {
    console.error("[lmk-chat-widget] LMK_CHAT_WIDGET_CONFIG.baseUrl is required");
    return;
  }

  var STYLE = [
    ".lmk-chat-widget * { box-sizing: border-box; }",
    ".lmk-chat-widget-toggle {",
    "  position: fixed; right: 20px; bottom: 20px; z-index: 2147483000;",
    "  width: 52px; height: 52px; border-radius: 999px; border: none;",
    "  background: #4f46e5; color: #fff; font-size: 22px; cursor: pointer;",
    "  box-shadow: 0 4px 14px rgba(0,0,0,0.25);",
    "}",
    ".lmk-chat-widget-panel {",
    "  position: fixed; right: 20px; bottom: 84px; z-index: 2147483000;",
    "  width: 320px; max-width: calc(100vw - 40px); height: 440px;",
    "  max-height: calc(100vh - 140px); display: none; flex-direction: column;",
    "  background: #fff; color: #111; border-radius: 12px; overflow: hidden;",
    "  box-shadow: 0 10px 40px rgba(0,0,0,0.3); font-family: system-ui, sans-serif;",
    "  border: 1px solid rgba(0,0,0,0.08);",
    "}",
    ".lmk-chat-widget-panel.lmk-open { display: flex; }",
    ".lmk-chat-widget-header {",
    "  padding: 10px 12px; font-size: 13px; font-weight: 600;",
    "  background: #4f46e5; color: #fff; display: flex; justify-content: space-between; align-items: center;",
    "}",
    ".lmk-chat-widget-close { background: none; border: none; color: #fff; cursor: pointer; font-size: 16px; line-height: 1; }",
    ".lmk-chat-widget-messages { flex: 1; overflow-y: auto; padding: 10px; font-size: 13px; }",
    ".lmk-chat-widget-msg { margin-bottom: 8px; padding: 7px 10px; border-radius: 8px; max-width: 85%; white-space: pre-wrap; word-break: break-word; }",
    ".lmk-chat-widget-msg.lmk-user { background: #eef2ff; margin-left: auto; }",
    ".lmk-chat-widget-msg.lmk-assistant { background: #f4f4f5; }",
    ".lmk-chat-widget-msg.lmk-error { background: #fef2f2; color: #991b1b; }",
    ".lmk-chat-widget-form { display: flex; gap: 6px; padding: 8px; border-top: 1px solid rgba(0,0,0,0.08); }",
    ".lmk-chat-widget-input {",
    "  flex: 1; border: 1px solid rgba(0,0,0,0.15); border-radius: 8px;",
    "  padding: 7px 9px; font-size: 13px; font-family: inherit;",
    "}",
    ".lmk-chat-widget-send { background: #4f46e5; color: #fff; border: none; border-radius: 8px; padding: 0 12px; font-size: 13px; cursor: pointer; }",
    ".lmk-chat-widget-send:disabled { opacity: 0.6; cursor: not-allowed; }",
  ].join("\n");

  function el(tag, className, text) {
    var node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined) node.textContent = text;
    return node;
  }

  function mount() {
    var style = el("style");
    style.textContent = STYLE;
    document.head.appendChild(style);

    var root = el("div", "lmk-chat-widget");

    var toggle = el("button", "lmk-chat-widget-toggle", "\u{1F4AC}");
    toggle.setAttribute("type", "button");
    toggle.setAttribute("aria-label", config.title || "Open chat");

    var panel = el("div", "lmk-chat-widget-panel");
    var header = el("div", "lmk-chat-widget-header");
    header.appendChild(el("span", "", config.title || "Chat"));
    var closeBtn = el("button", "lmk-chat-widget-close", "✕");
    closeBtn.setAttribute("type", "button");
    closeBtn.setAttribute("aria-label", "Close chat");
    header.appendChild(closeBtn);
    panel.appendChild(header);

    var messages = el("div", "lmk-chat-widget-messages");
    panel.appendChild(messages);

    var form = el("form", "lmk-chat-widget-form");
    var input = el("input", "lmk-chat-widget-input");
    input.type = "text";
    input.placeholder = "Message…";
    input.autocomplete = "off";
    var sendBtn = el("button", "lmk-chat-widget-send", "Send");
    sendBtn.type = "submit";
    form.appendChild(input);
    form.appendChild(sendBtn);
    panel.appendChild(form);

    root.appendChild(toggle);
    root.appendChild(panel);
    document.body.appendChild(root);

    var history = [];
    if (config.systemPrompt) {
      history.push({ role: "system", content: String(config.systemPrompt) });
    }

    function appendMessage(role, text) {
      var bubble = el("div", "lmk-chat-widget-msg lmk-" + role, text);
      messages.appendChild(bubble);
      messages.scrollTop = messages.scrollHeight;
      return bubble;
    }

    function setOpen(open) {
      panel.classList.toggle("lmk-open", open);
    }

    toggle.addEventListener("click", function () {
      setOpen(!panel.classList.contains("lmk-open"));
    });
    closeBtn.addEventListener("click", function () {
      setOpen(false);
    });

    form.addEventListener("submit", function (event) {
      event.preventDefault();
      var text = input.value.trim();
      if (!text || sendBtn.disabled) return;
      input.value = "";
      history.push({ role: "user", content: text });
      appendMessage("user", text);
      sendBtn.disabled = true;

      var headers = { "Content-Type": "application/json" };
      if (config.token) {
        headers.Authorization = "Bearer " + config.token;
      }

      fetch(config.baseUrl.replace(/\/+$/, "") + "/chat/completions", {
        method: "POST",
        headers: headers,
        body: JSON.stringify({
          model: config.model || "default",
          messages: history,
          stream: false,
        }),
      })
        .then(function (response) {
          return response
            .text()
            .then(function (text) {
              var parsed;
              try {
                parsed = JSON.parse(text);
              } catch (e) {
                parsed = null;
              }
              if (!response.ok) {
                var message =
                  (parsed && parsed.error && parsed.error.message) ||
                  "Request failed with " + response.status;
                throw new Error(message);
              }
              return parsed;
            });
        })
        .then(function (data) {
          var reply =
            data &&
            data.choices &&
            data.choices[0] &&
            data.choices[0].message &&
            data.choices[0].message.content;
          if (typeof reply === "string") {
            history.push({ role: "assistant", content: reply });
            appendMessage("assistant", reply);
          } else {
            appendMessage("error", "No reply content in response.");
          }
        })
        .catch(function (error) {
          appendMessage("error", error && error.message ? error.message : String(error));
        })
        .then(function () {
          sendBtn.disabled = false;
        });
    });
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", mount);
  } else {
    mount();
  }
})();
