// The service worker of knixx lights: it keeps the app's files, so that the
// installed app also starts without a network; the board is reached over
// Bluetooth, not the internet.
//
// Network first: online, every file comes fresh from the server and is kept;
// offline, the kept one is served. trunk names the script, wasm and style
// files after their hash, so each new page drops the kept files it no
// longer names.

const CACHE = "knixx-lights-v1";

self.addEventListener("install", () => self.skipWaiting());

self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((keys) => Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k))))
      .then(() => self.clients.claim()),
  );
});

self.addEventListener("fetch", (event) => {
  const req = event.request;
  if (req.method !== "GET" || new URL(req.url).origin !== self.location.origin) {
    return;
  }
  event.respondWith(fromNetwork(req, event));
});

async function fromNetwork(req, event) {
  const cache = await caches.open(CACHE);
  try {
    const res = await fetch(req);
    if (res.ok) {
      await cache.put(req, res.clone());
      if (req.mode === "navigate") {
        event.waitUntil(res.clone().text().then((html) => prune(cache, html)));
      }
    }
    return res;
  } catch (err) {
    const kept = await cache.match(req, { ignoreSearch: req.mode === "navigate" });
    if (kept) {
      return kept;
    }
    throw err;
  }
}

// Drops the kept hashed files (name-<hash>.ext) that the page html no longer
// names.
async function prune(cache, html) {
  for (const req of await cache.keys()) {
    const name = new URL(req.url).pathname.split("/").pop();
    if (/-[0-9a-f]{16}(_bg)?\.(js|wasm|css)$/.test(name) && !html.includes(name)) {
      await cache.delete(req);
    }
  }
}
