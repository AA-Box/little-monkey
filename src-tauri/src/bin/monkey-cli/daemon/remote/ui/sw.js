// The controller's service worker. Its only job is push: showing a
// notification when the runner has something worth waking this device for, and
// focusing the controller when the notification is tapped.
//
// Deliberately not a cache: the controller is served by the runner over the
// pinned connection it is pairing with, and a stale cached copy of a security
// surface is worse than a page that does not load. Offline state is handled in
// `app.js`, against IndexedDB, where it can be marked stale and have its
// side-effecting controls disabled.
//
// Served from the origin root so its scope covers the whole controller.

self.addEventListener("install", () => {
  // Take over immediately: a device that has just subscribed should be able to
  // receive the first notification without a reload.
  self.skipWaiting();
});

self.addEventListener("activate", (event) => {
  event.waitUntil(self.clients.claim());
});

self.addEventListener("push", (event) => {
  // A payload-less push is a wake-up from a push service, not from the runner.
  // Nothing is invented for it: showing "something happened" with no idea what
  // would be a notification the user cannot act on.
  if (!event.data) return;
  let payload;
  try {
    payload = event.data.json();
  } catch {
    return;
  }
  const title = typeof payload.title === "string" && payload.title ? payload.title : "Little Monkey";
  const body = typeof payload.body === "string" ? payload.body : "";
  const data = payload.data && typeof payload.data === "object" ? payload.data : {};
  event.waitUntil(
    self.registration.showNotification(title, {
      body,
      // Tagged by what happened, so a phone that was off for an hour shows one
      // approval notification rather than forty.
      tag: `little-monkey-${data.kind || "event"}`,
      renotify: true,
      data,
    }),
  );
});

self.addEventListener("notificationclick", (event) => {
  event.notification.close();
  // Only ever this origin's own controller. The notification payload carries
  // ids, never URLs, so there is nothing here that could navigate the user
  // somewhere the runner did not serve.
  const target = new URL("/", self.registration.scope);
  event.waitUntil(
    self.clients.matchAll({ type: "window", includeUncontrolled: true }).then((clients) => {
      for (const client of clients) {
        if (new URL(client.url).origin === target.origin && "focus" in client) {
          return client.focus();
        }
      }
      return self.clients.openWindow ? self.clients.openWindow(target.href) : undefined;
    }),
  );
});
