// Peridot share viewer. Link: /s#1.<sha256>.<server host>.<key>
// Fetches the encrypted blob by its hash, checks the hash, decrypts with
// AES-256-GCM using the key from the fragment, and shows the file.
"use strict";
(function () {
  const $ = (id) => document.getElementById(id);
  const say = (text, isError) => {
    $("status").hidden = false;
    $("message").textContent = text;
    $("message").className = isError ? "error" : "";
  };

  function parseLink(hash) {
    const parts = hash.replace(/^#/, "").split(".");
    if (parts.length < 4 || parts[0] !== "1") return null;
    const sha = parts[1];
    if (!/^[0-9a-f]{64}$/.test(sha)) return null;
    const keyB64 = parts[parts.length - 1];
    const server = parts.slice(2, -1).join(".");
    if (!/^[A-Za-z0-9.-]+(:[0-9]+)?$/.test(server) || !server.includes(".")) return null;
    const key = b64url(keyB64);
    if (!key || key.length !== 32) return null;
    return { sha, server, key };
  }

  function b64url(s) {
    try {
      const b = atob(s.replace(/-/g, "+").replace(/_/g, "/") + "===".slice((s.length + 3) % 4));
      return Uint8Array.from(b, (c) => c.charCodeAt(0));
    } catch (e) { return null; }
  }

  function hex(buf) {
    return Array.from(new Uint8Array(buf), (b) => b.toString(16).padStart(2, "0")).join("");
  }

  function human(n) {
    if (n < 1024) return n + " B";
    if (n < 1048576) return (n / 1024).toFixed(1) + " KB";
    return (n / 1048576).toFixed(1) + " MB";
  }

  async function open() {
    const link = parseLink(location.hash);
    if (!link) { say("This isn't a complete Peridot link. Ask the sender to copy it again.", true); return; }

    say("Fetching…");
    let res;
    try {
      // Servers are https; a loopback one (development) is http.
      const host = link.server.replace(/:[0-9]+$/, "");
      const scheme = (host === "127.0.0.1" || host === "localhost") ? "http" : "https";
      res = await fetch(scheme + "://" + link.server + "/" + link.sha, { referrerPolicy: "no-referrer", credentials: "omit" });
    } catch (e) {
      say("Couldn't reach the server holding this file.", true); return;
    }
    if (res.status === 404 || res.status === 410) { say("This link has expired or the file was removed by the sender.", true); return; }
    if (!res.ok) { say("The server holding this file answered " + res.status + ".", true); return; }
    const blob = new Uint8Array(await res.arrayBuffer());

    // The link names the blob by its hash: anything else is not the file.
    const digest = hex(await crypto.subtle.digest("SHA-256", blob));
    if (digest !== link.sha) { say("The file doesn't match this link.", true); return; }

    say("Decrypting…");
    let plain;
    try {
      const key = await crypto.subtle.importKey("raw", link.key, { name: "AES-GCM" }, false, ["decrypt"]);
      plain = new Uint8Array(await crypto.subtle.decrypt({ name: "AES-GCM", iv: blob.slice(0, 12) }, key, blob.slice(12)));
    } catch (e) {
      say("The key in this link doesn't open this file.", true); return;
    }
    if (plain.length < 8 || String.fromCharCode(...plain.slice(0, 4)) !== "PDS1") { say("This isn't a Peridot file.", true); return; }
    const len = new DataView(plain.buffer).getUint32(4);
    let header;
    try { header = JSON.parse(new TextDecoder().decode(plain.slice(8, 8 + len))); } catch (e) { say("This isn't a Peridot file.", true); return; }
    const data = plain.slice(8 + len);
    show(header, data);
  }

  function show(header, data) {
    const name = String(header.name || "file").slice(0, 200);
    const mime = /^[a-z]+\/[a-z0-9.+-]+$/i.test(header.type || "") ? header.type : "application/octet-stream";
    const url = URL.createObjectURL(new Blob([data], { type: mime }));
    $("status").hidden = true;
    $("file").hidden = false;
    $("name").textContent = name;
    $("size").textContent = human(data.length);
    const dl = $("download");
    dl.href = url;
    dl.download = name;
    const view = $("view");
    view.textContent = "";
    let el = null;
    if (mime.startsWith("image/") && mime !== "image/svg+xml") {
      el = document.createElement("img"); el.src = url; el.alt = name;
    } else if (mime.startsWith("video/")) {
      el = document.createElement("video"); el.src = url; el.controls = true;
    } else if (mime.startsWith("audio/")) {
      el = document.createElement("audio"); el.src = url; el.controls = true;
    } else if (mime === "application/pdf") {
      el = document.createElement("iframe"); el.src = url; el.title = name;
    } else if (mime.startsWith("text/") || mime === "application/json") {
      el = document.createElement("pre");
      el.textContent = new TextDecoder().decode(data.slice(0, 2 * 1024 * 1024));
    }
    if (el) view.appendChild(el);
    else {
      const p = document.createElement("p");
      p.textContent = "This kind of file can't be shown here. Use Download.";
      view.appendChild(p);
    }
  }

  window.addEventListener("hashchange", () => location.reload());
  open();
})();
