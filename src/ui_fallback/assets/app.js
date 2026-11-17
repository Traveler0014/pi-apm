// Built-in fallback page: proves the ticket -> token flow end to end.
// The one-time ticket is read from location.hash, exchanged for the real token
// (kept in memory only), and the fragment is stripped immediately so neither
// value can leak through history, referrers or screenshots.
(() => {
  const status = document.getElementById("status");
  const summary = document.getElementById("summary");
  let token = null;

  const hash = window.location.hash.replace(/^#/, "");
  let ticket = null;
  for (const pair of hash.split("&")) {
    if (pair.startsWith("ticket=")) {
      ticket = decodeURIComponent(pair.slice("ticket=".length));
    }
  }
  if (ticket) {
    history.replaceState(null, "", location.pathname + location.search);
  }

  const fail = (message) => {
    status.textContent = message;
    status.dataset.state = "error";
  };

  if (!ticket) {
    fail("No bootstrap ticket in the URL. Open the URL printed by `apm ui`.");
    return;
  }

  fetch("/api/bootstrap", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ ticket }),
    cache: "no-store",
  })
    .then((response) => {
      if (!response.ok) throw new Error(`bootstrap ${response.status}`);
      return response.json();
    })
    .then((session) => {
      token = session.token;
      return fetch("/api/dashboard", {
        headers: { Authorization: `Bearer ${token}` },
        cache: "no-store",
      });
    })
    .then((response) => {
      if (!response.ok) throw new Error(`dashboard ${response.status}`);
      return response.json();
    })
    .then((body) => {
      const totals = (body.dashboard && body.dashboard.totals) || {};
      const entries = Object.entries(totals).filter(([key]) => key !== "generated_at");
      status.textContent = `Connected — ${entries.length} ledger counters.`;
      status.dataset.state = "ok";
      for (const [key, value] of entries) {
        const li = document.createElement("li");
        li.textContent = `${key}: ${value}`;
        summary.appendChild(li);
      }
    })
    .catch((error) => {
      fail(`Could not start the dashboard session (${error.message}). Re-run \`apm ui\` for a fresh URL.`);
    });
})();
