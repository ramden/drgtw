// DRGTW demo portal — plain JS, no build step, no CDN deps.
//
// Calls the gateway SAME-ORIGIN at /v1/chat/completions (nginx proxies it to
// gateway:8080), so no CORS is involved. Each turn sends the FULL chat history
// with `x-drgtw-debug: on`. The conversation reads like a normal chat (user +
// assistant bubbles); between them an always-visible "behind the scenes" wire
// view shows what the provider actually received (pseudonymized) and the raw
// response before DRGTW restored it.

(function () {
  "use strict";

  var API_URL = "/v1/chat/completions";
  var API_KEY = "sk-drgtw-demo-key";
  var MODEL = "gpt-4o-mini";

  var history = [];

  var elChat = document.getElementById("chat");
  var elEmptyHint = document.getElementById("empty-hint");
  var elForm = document.getElementById("composer");
  var elInput = document.getElementById("input");
  var elSend = document.getElementById("send");
  var elStatus = document.getElementById("status-pill");

  function setStatus(text, cls) {
    elStatus.textContent = text;
    elStatus.className = "pill " + cls;
  }

  // ----- DOM helpers --------------------------------------------------------

  function el(tag, cls, text) {
    var n = document.createElement(tag);
    if (cls) n.className = cls;
    if (text !== undefined && text !== null) n.textContent = text;
    return n;
  }

  function scrollDown() { elChat.scrollTop = elChat.scrollHeight; }

  // Render text into a node, wrapping placeholder tokens (EMAIL_1, PERSON_2,
  // CARD_1, TICKET_1, also vault-style EMAIL_a1b2c3...) in a highlight span.
  var PH_RE = /\b([A-Z][A-Z0-9]*_[0-9a-f]+)\b/g;
  function renderWithPlaceholders(node, text) {
    node.textContent = "";
    var last = 0, m;
    PH_RE.lastIndex = 0;
    while ((m = PH_RE.exec(text)) !== null) {
      if (m.index > last) node.appendChild(document.createTextNode(text.slice(last, m.index)));
      node.appendChild(el("span", "ph", m[0]));
      last = m.index + m[0].length;
    }
    if (last < text.length) node.appendChild(document.createTextNode(text.slice(last)));
  }

  function stringifyContent(content) {
    if (typeof content === "string") return content;
    if (Array.isArray(content)) {
      return content
        .map(function (part) {
          if (typeof part === "string") return part;
          if (part && typeof part.text === "string") return part.text;
          return "";
        })
        .join("");
    }
    return content == null ? "" : String(content);
  }

  function lastUserContent(requestBody) {
    if (!requestBody || !Array.isArray(requestBody.messages)) return null;
    for (var i = requestBody.messages.length - 1; i >= 0; i--) {
      var m = requestBody.messages[i];
      if (m && m.role === "user") return stringifyContent(m.content);
    }
    return null;
  }

  // ----- rendering ----------------------------------------------------------

  function clearHint() {
    if (elEmptyHint) { elEmptyHint.remove(); elEmptyHint = null; }
  }

  // user bubble (normal chat) + the wire view placeholder for this turn.
  function renderTurn(typedText) {
    clearHint();

    elChat.appendChild(el("div", "bubble bubble-user", typedText));

    var wire = el("div", "wire");

    var meta = el("div", "wire-meta");
    meta.appendChild(document.createTextNode("behind the scenes"));
    var badge = el("span", "badge", "entities: …");
    meta.appendChild(badge);
    wire.appendChild(meta);

    var sentRow = el("div", "wire-row");
    sentRow.appendChild(el("div", "wire-dir dir-sent", "sent to provider"));
    var sentText = el("div", "wire-text empty", "waiting…");
    sentRow.appendChild(sentText);
    wire.appendChild(sentRow);

    var rawRow = el("div", "wire-row");
    rawRow.appendChild(el("div", "wire-dir dir-raw", "raw response"));
    var rawText = el("div", "wire-text empty", "waiting…");
    rawRow.appendChild(rawText);
    wire.appendChild(rawRow);

    elChat.appendChild(wire);
    scrollDown();

    return { wire: wire, badge: badge, sentText: sentText, rawText: rawText };
  }

  function fillWire(node, text) {
    node.classList.remove("empty");
    renderWithPlaceholders(node, text);
  }

  function setBadge(badge, count) {
    badge.textContent = "entities: " + count;
    badge.className = "badge" + (count > 0 ? " badge-active" : "");
  }

  function addAssistantBubble(text, isError) {
    var b = el("div", "bubble " + (isError ? "bubble-error" : "bubble-assistant"), text);
    elChat.appendChild(b);
    scrollDown();
  }

  // ----- networking ---------------------------------------------------------

  function errorMessageFrom(status, payload, rawText) {
    if (payload && payload.error) {
      var e = payload.error;
      var parts = [];
      if (e.type) parts.push(e.type);
      if (e.code && e.code !== e.type) parts.push(e.code);
      var prefix = parts.length ? "[" + parts.join(" / ") + "] " : "";
      return "HTTP " + status + " " + prefix + (e.message || rawText || "request failed");
    }
    var hint = "";
    if (status === 401) hint = " — check the virtual key / provider key";
    else if (status === 429) hint = " — rate limited or quota exhausted";
    return "HTTP " + status + hint + (rawText ? ": " + rawText.slice(0, 400) : "");
  }

  async function send(message) {
    var turn = renderTurn(message);
    setStatus("sending…", "pill-busy");
    elSend.disabled = true;
    elInput.disabled = true;

    history.push({ role: "user", content: message });

    try {
      var resp = await fetch(API_URL, {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          "Authorization": "Bearer " + API_KEY,
          "x-drgtw-debug": "on"
        },
        body: JSON.stringify({ model: MODEL, messages: history, stream: false })
      });

      var rawText = await resp.text();
      var payload = null;
      try { payload = JSON.parse(rawText); } catch (_) { /* non-JSON */ }

      if (!resp.ok) {
        history.pop();
        turn.sentText.textContent = "—"; turn.sentText.classList.remove("empty");
        turn.rawText.textContent = "—"; turn.rawText.classList.remove("empty");
        addAssistantBubble(errorMessageFrom(resp.status, payload, rawText), true);
        setStatus("error", "pill-error");
        return;
      }
      if (!payload) {
        history.pop();
        addAssistantBubble("Gateway returned a non-JSON response: " + rawText.slice(0, 400), true);
        setStatus("error", "pill-error");
        return;
      }

      var dbg = payload.drgtw_debug || null;

      if (dbg && dbg.pseudonymized_request) {
        var sent = lastUserContent(dbg.pseudonymized_request);
        fillWire(turn.sentText, sent != null ? sent : JSON.stringify(dbg.pseudonymized_request));
      } else {
        turn.sentText.textContent = "(debug off — could not capture)";
      }

      if (dbg && Array.isArray(dbg.raw_response_text) && dbg.raw_response_text.length) {
        fillWire(turn.rawText, dbg.raw_response_text.join("\n---\n"));
      } else {
        turn.rawText.textContent = "(no raw response captured)";
      }

      setBadge(turn.badge, dbg && typeof dbg.entities === "number" ? dbg.entities : 0);

      var assistant = "";
      if (payload.choices && payload.choices[0] && payload.choices[0].message) {
        assistant = stringifyContent(payload.choices[0].message.content);
      }
      addAssistantBubble(assistant || "(empty response)", false);
      history.push({ role: "assistant", content: assistant });

      setStatus("ready", "pill-idle");
    } catch (err) {
      history.pop();
      addAssistantBubble(
        "Network error: " + (err && err.message ? err.message : String(err)) +
        " — is the gateway up? (docker compose up --build)", true);
      setStatus("error", "pill-error");
    } finally {
      elSend.disabled = false;
      elInput.disabled = false;
      elInput.focus();
    }
  }

  // ----- wiring -------------------------------------------------------------

  elForm.addEventListener("submit", function (e) {
    e.preventDefault();
    var message = elInput.value.trim();
    if (!message) return;
    elInput.value = "";
    send(message);
  });

  elInput.focus();
})();
