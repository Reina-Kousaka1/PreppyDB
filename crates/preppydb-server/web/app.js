/* PreppyDB administration UI.
 *
 * No frameworks, no external requests, no cookies: the credential is held in
 * sessionStorage for the duration of the tab only and sent as a Bearer header.
 */
(function () {
  "use strict";

  var state = {
    token: sessionStorage.getItem("preppydb.token") || "",
    principal: null,
    collection: "",
  };

  function el(id) { return document.getElementById(id); }
  function text(node, value) { node.textContent = value == null ? "" : String(value); }

  function setStatus(node, message, isError) {
    if (!node) { return; }
    node.hidden = false;
    node.textContent = message;
    node.className = isError ? "error" : "ok";
  }

  function api(method, path, body) {
    var options = { method: method, headers: {} };
    if (state.token) { options.headers.Authorization = "Bearer " + state.token; }
    if (body !== undefined) {
      options.headers["Content-Type"] = "application/json";
      options.body = JSON.stringify(body);
    }
    return fetch(path, options).then(function (response) {
      return response.text().then(function (payload) {
        var parsed = null;
        try { parsed = payload ? JSON.parse(payload) : null; } catch (err) { parsed = null; }
        if (!response.ok) {
          var message = parsed && parsed.error ? parsed.error.message : response.statusText;
          var error = new Error(message || ("HTTP " + response.status));
          error.status = response.status;
          error.payload = parsed;
          throw error;
        }
        return parsed;
      });
    });
  }

  function showLogin() {
    el("login-view").hidden = false;
    el("app-view").hidden = true;
    el("signout").hidden = true;
    text(el("who"), "not signed in");
  }

  function showApp(principal) {
    state.principal = principal;
    el("login-view").hidden = true;
    el("app-view").hidden = false;
    el("signout").hidden = false;
    text(el("who"), principal.name + " (" + principal.roles.join(", ") + ")");
    refreshOverview();
  }

  function signIn(token) {
    state.token = token;
    sessionStorage.setItem("preppydb.token", token);
    return api("GET", "/v1/auth/whoami")
      .then(function (response) { showApp(response.data); })
      .catch(function (err) {
        state.token = "";
        sessionStorage.removeItem("preppydb.token");
        setStatus(el("login-error"), "Sign in failed: " + err.message, true);
        showLogin();
      });
  }

  el("login-form").addEventListener("submit", function (event) {
    event.preventDefault();
    var token = el("token").value.trim();
    var username = el("username").value.trim();
    var password = el("password").value;
    el("login-error").hidden = true;
    if (token) {
      signIn(token);
      return;
    }
    if (!username || !password) {
      setStatus(el("login-error"), "Enter a token, or a username and password.", true);
      return;
    }
    api("POST", "/v1/auth/login", { username: username, password: password })
      .then(function (response) { return signIn(response.data.token); })
      .catch(function (err) { setStatus(el("login-error"), "Sign in failed: " + err.message, true); });
  });

  el("use-token").addEventListener("click", function () {
    el("username").value = "";
    el("password").value = "";
    el("token").focus();
  });

  el("signout").addEventListener("click", function () {
    api("POST", "/v1/auth/logout", {}).catch(function () { /* best effort */ })
      .then(function () {
        state.token = "";
        sessionStorage.removeItem("preppydb.token");
        showLogin();
      });
  });

  /* ------------------------------------------------------------- tabs */
  Array.prototype.forEach.call(document.querySelectorAll(".tab"), function (tab) {
    tab.addEventListener("click", function () {
      Array.prototype.forEach.call(document.querySelectorAll(".tab"), function (other) {
        other.classList.toggle("active", other === tab);
      });
      Array.prototype.forEach.call(document.querySelectorAll(".tab-panel"), function (panel) {
        panel.hidden = panel.id !== "tab-" + tab.dataset.tab;
      });
      if (tab.dataset.tab === "collections") { loadCollections(); }
      if (tab.dataset.tab === "pricing") { loadRules(); }
      if (tab.dataset.tab === "maintenance") { loadRecovery(); }
    });
  });

  /* ------------------------------------------------------------- overview */
  function card(label, value) {
    var div = document.createElement("div");
    div.className = "card";
    var l = document.createElement("div");
    l.className = "label";
    text(l, label);
    var v = document.createElement("div");
    v.className = "value";
    text(v, value);
    div.appendChild(l);
    div.appendChild(v);
    return div;
  }

  function refreshOverview() {
    api("GET", "/v1/stats").then(function (response) {
      var stats = response.data;
      var grid = el("overview");
      grid.textContent = "";
      grid.appendChild(card("Documents", stats.total_documents));
      grid.appendChild(card("Last sequence", stats.last_seq));
      grid.appendChild(card("Generation", stats.generation));
      grid.appendChild(card("WAL bytes", stats.wal_bytes));
      grid.appendChild(card("Snapshot bytes", stats.snapshot_bytes));
      grid.appendChild(card("Disk bytes", stats.disk_bytes));
      grid.appendChild(card("Sync mode", stats.sync_mode));
      grid.appendChild(card("Uptime (s)", Math.round(stats.uptime_ms / 1000)));

      var body = el("overview-collections").querySelector("tbody");
      body.textContent = "";
      stats.collections.forEach(function (collection) {
        var row = document.createElement("tr");
        [collection.name, collection.documents, collection.live_documents,
         collection.expired_documents, collection.indexes.join(", "),
         collection.has_schema ? "yes" : "no"].forEach(function (value, index) {
          var cell = document.createElement("td");
          if (index === 0) { cell.className = "mono"; }
          text(cell, value);
          row.appendChild(cell);
        });
        body.appendChild(row);
      });
    }).catch(function (err) { setStatus(el("login-error"), err.message, true); });
  }
  el("refresh-overview").addEventListener("click", refreshOverview);

  /* ------------------------------------------------------------- collections */
  function loadCollections() {
    api("GET", "/v1/collections").then(function (response) {
      var list = el("collection-list");
      list.textContent = "";
      response.data.collections.forEach(function (collection) {
        var item = document.createElement("li");
        var button = document.createElement("button");
        text(button, collection.name);
        button.className = collection.name === state.collection ? "primary" : "";
        button.addEventListener("click", function () {
          state.collection = collection.name;
          text(el("current-collection"), collection.name);
          el("doc-editor").hidden = true;
          loadCollections();
          scanDocuments();
        });
        item.appendChild(button);
        list.appendChild(item);
      });
    }).catch(function (err) { alert(err.message); });
  }

  el("create-collection").addEventListener("click", function () {
    var name = el("new-collection").value.trim();
    if (!name) { return; }
    var schemaText = el("new-schema").value.trim();
    var body = { name: name };
    if (schemaText) {
      try { body.schema = JSON.parse(schemaText); }
      catch (err) { alert("Schema is not valid JSON: " + err.message); return; }
    }
    api("POST", "/v1/collections", body).then(function () {
      el("new-collection").value = "";
      el("new-schema").value = "";
      loadCollections();
    }).catch(function (err) { alert(err.message); });
  });

  function scanDocuments() {
    if (!state.collection) { return; }
    var prefix = el("doc-prefix").value;
    var path = "/v1/collections/" + encodeURIComponent(state.collection) + "/documents?limit=50";
    if (prefix) { path += "&prefix=" + encodeURIComponent(prefix); }
    api("GET", path).then(function (response) {
      var body = el("documents").querySelector("tbody");
      body.textContent = "";
      response.data.documents.forEach(function (doc) {
        var row = document.createElement("tr");
        var keyCell = document.createElement("td");
        keyCell.className = "mono";
        text(keyCell, doc.key);
        var versionCell = document.createElement("td");
        text(versionCell, doc.version);
        var valueCell = document.createElement("td");
        valueCell.className = "mono";
        text(valueCell, JSON.stringify(doc.value).slice(0, 240));
        var actionCell = document.createElement("td");
        var edit = document.createElement("button");
        text(edit, "Edit");
        edit.addEventListener("click", function () {
          el("doc-editor").hidden = false;
          el("doc-key").value = doc.key;
          el("doc-body").value = JSON.stringify(doc.value, null, 2);
          el("doc-expected").value = String(doc.version);
        });
        var remove = document.createElement("button");
        text(remove, "Delete");
        remove.className = "danger";
        remove.addEventListener("click", function () {
          if (!confirm("Delete '" + doc.key + "' from '" + state.collection + "'?")) { return; }
          api("DELETE", "/v1/collections/" + encodeURIComponent(state.collection) +
              "/documents/" + encodeURIComponent(doc.key))
            .then(scanDocuments)
            .catch(function (err) { alert(err.message); });
        });
        actionCell.appendChild(edit);
        actionCell.appendChild(remove);
        row.appendChild(keyCell);
        row.appendChild(versionCell);
        row.appendChild(valueCell);
        row.appendChild(actionCell);
        body.appendChild(row);
      });
    }).catch(function (err) { alert(err.message); });
  }
  el("scan-docs").addEventListener("click", scanDocuments);

  el("new-doc").addEventListener("click", function () {
    if (!state.collection) { alert("Select a collection first."); return; }
    el("doc-editor").hidden = false;
    el("doc-key").value = "";
    el("doc-body").value = '{\n  \n}';
    el("doc-expected").value = "0";
  });

  el("cancel-doc").addEventListener("click", function () { el("doc-editor").hidden = true; });

  el("save-doc").addEventListener("click", function () {
    var key = el("doc-key").value.trim();
    var body;
    try { body = JSON.parse(el("doc-body").value); }
    catch (err) { alert("Document is not valid JSON: " + err.message); return; }
    var expected = el("doc-expected").value.trim();
    var path = "/v1/collections/" + encodeURIComponent(state.collection) +
      "/documents/" + encodeURIComponent(key);
    var payload = { value: body };
    if (expected !== "") { payload.expected_version = Number(expected); }
    api("PUT", path, payload).then(function () {
      el("doc-editor").hidden = true;
      scanDocuments();
    }).catch(function (err) { alert(err.message); });
  });

  /* ------------------------------------------------------------- pricing */
  function loadRules() {
    api("GET", "/v1/pricing/rules").then(function (response) {
      var body = el("rules").querySelector("tbody");
      body.textContent = "";
      response.data.rules.forEach(function (rule) {
        var row = document.createElement("tr");
        [rule.item_id, rule.current_price_cents, rule.min_price_cents,
         rule.max_price_cents, rule.version].forEach(function (value, index) {
          var cell = document.createElement("td");
          if (index === 0) { cell.className = "mono"; }
          text(cell, value);
          row.appendChild(cell);
        });
        var action = document.createElement("td");
        var tick = document.createElement("button");
        text(tick, "Run tick");
        tick.addEventListener("click", function () {
          api("POST", "/v1/pricing/rules/" + encodeURIComponent(rule.item_id) + "/tick", {})
            .then(function (response) {
              alert("New price: " + response.data.change.new_cents + " cents");
              loadRules();
            })
            .catch(function (err) { alert(err.message); });
        });
        action.appendChild(tick);
        row.appendChild(action);
        body.appendChild(row);
      });
    }).catch(function (err) { alert(err.message); });
  }
  el("refresh-rules").addEventListener("click", loadRules);

  el("save-rule").addEventListener("click", function () {
    var item = el("rule-item").value.trim();
    if (!item) { return; }
    var payload = {
      item_id: item,
      min_price_cents: Number(el("rule-min").value),
      max_price_cents: Number(el("rule-max").value),
      anchor_price_cents: Number(el("rule-anchor").value),
    };
    api("PUT", "/v1/pricing/rules/" + encodeURIComponent(item), payload)
      .then(loadRules)
      .catch(function (err) { alert(err.message); });
  });

  el("load-history").addEventListener("click", function () {
    var item = el("history-item").value.trim();
    if (!item) { return; }
    api("GET", "/v1/pricing/history/" + encodeURIComponent(item)).then(function (response) {
      var body = el("history").querySelector("tbody");
      body.textContent = "";
      response.data.history.forEach(function (entry) {
        var row = document.createElement("tr");
        [entry.item_id, entry.price_cents, entry.previous_cents, entry.rule_version, entry.at]
          .forEach(function (value) {
            var cell = document.createElement("td");
            text(cell, value);
            row.appendChild(cell);
          });
        body.appendChild(row);
      });
    }).catch(function (err) { alert(err.message); });
  });

  el("verify-audit").addEventListener("click", function () {
    api("GET", "/v1/pricing/audit/verify").then(function (response) {
      el("audit-out").textContent = JSON.stringify(response.data, null, 2);
    }).catch(function (err) { el("audit-out").textContent = err.message; });
  });

  el("load-audit").addEventListener("click", function () {
    api("GET", "/v1/pricing/audit?limit=20").then(function (response) {
      el("audit-out").textContent = JSON.stringify(response.data.entries, null, 2);
    }).catch(function (err) { el("audit-out").textContent = err.message; });
  });

  /* ------------------------------------------------------------- personality */
  el("bootstrap").addEventListener("click", function () {
    api("POST", "/v1/personality/bootstrap", {}).then(function (response) {
      el("personality-out").textContent = JSON.stringify(response.data, null, 2);
    }).catch(function (err) { el("personality-out").textContent = err.message; });
  });

  el("personality-overview").addEventListener("click", function () {
    api("GET", "/v1/personality/overview").then(function (response) {
      el("personality-out").textContent = JSON.stringify(response.data, null, 2);
    }).catch(function (err) { el("personality-out").textContent = err.message; });
  });

  /* ------------------------------------------------------------- maintenance */
  function maintenance(path, label) {
    if (!confirm("Run " + label + " now?")) { return; }
    api("POST", path, {}).then(function (response) {
      el("maintenance-out").textContent = JSON.stringify(response.data, null, 2);
    }).catch(function (err) { el("maintenance-out").textContent = err.message; });
  }
  el("checkpoint").addEventListener("click", function () {
    maintenance("/v1/admin/checkpoint", "a checkpoint");
  });
  el("compact").addEventListener("click", function () {
    maintenance("/v1/admin/compact", "a compaction");
  });
  el("sweep").addEventListener("click", function () {
    maintenance("/v1/admin/sweep-expired", "an expired-document sweep");
  });
  el("verify").addEventListener("click", function () {
    api("POST", "/v1/admin/verify", {}).then(function (response) {
      el("maintenance-out").textContent = JSON.stringify(response.data, null, 2);
    }).catch(function (err) { el("maintenance-out").textContent = err.message; });
  });

  el("backup").addEventListener("click", function () {
    var path = el("backup-path").value.trim();
    if (!path) { return; }
    api("POST", "/v1/admin/backup", { path: path }).then(function (response) {
      el("maintenance-out").textContent = JSON.stringify(response.data, null, 2);
    }).catch(function (err) { el("maintenance-out").textContent = err.message; });
  });

  function loadRecovery() {
    api("GET", "/v1/admin/recovery").then(function (response) {
      el("recovery-out").textContent = JSON.stringify(response.data, null, 2);
    }).catch(function (err) { el("recovery-out").textContent = err.message; });
  }

  /* ------------------------------------------------------------- boot */
  api("GET", "/v1/version").then(function (response) {
    text(el("version"), response.data.version);
  }).catch(function () { /* ignore */ });

  if (state.token) {
    api("GET", "/v1/auth/whoami")
      .then(function (response) { showApp(response.data); })
      .catch(function () {
        state.token = "";
        sessionStorage.removeItem("preppydb.token");
        showLogin();
      });
  } else {
    showLogin();
  }
})();
