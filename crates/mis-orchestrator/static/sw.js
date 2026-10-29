// Service worker: shows the orchestrator's push notifications and opens their task.
// Payload: { title, body, tag: task id, url: "/#task/<id>" } (see push.rs).
'use strict';

self.addEventListener('install', () => self.skipWaiting());
self.addEventListener('activate', (e) => e.waitUntil(self.clients.claim()));

self.addEventListener('push', (e) => {
  let p = {};
  try { p = e.data ? e.data.json() : {}; } catch {}
  // Always show one: browsers penalise (Safari: revoke) pushes that show nothing.
  // The tag makes a repeat for the same task replace the earlier one.
  e.waitUntil(self.registration.showNotification(p.title || 'Make It So', {
    body: p.body || '',
    tag: p.tag,
    icon: '/apple-touch-icon.png',
    data: { url: p.url || '/' },
  }));
});

// Focus an open window and have it show the task (a hash change, no reload), else open one.
self.addEventListener('notificationclick', (e) => {
  e.notification.close();
  const url = new URL((e.notification.data && e.notification.data.url) || '/', self.location.origin).href;
  e.waitUntil((async () => {
    const wins = await self.clients.matchAll({ type: 'window', includeUncontrolled: true });
    const win = wins.find((w) => w.url.startsWith(self.location.origin));
    if (!win) return self.clients.openWindow(url);
    win.postMessage({ open: url });
    return win.focus();
  })());
});
