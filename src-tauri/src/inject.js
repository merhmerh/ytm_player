// Injected into music.youtube.com. Exposes player controls to the app and
// reports what's playing so it can be restored after a restart or reload.
(() => {
  if (window.top !== window || location.hostname !== 'music.youtube.com' || window.__ytmd) return;

  // YouTube Music asks "Leave site?" on reload; that would block the watchdog's reloads.
  // This runs before YTM's own scripts, so it's first in line and can stop its handler.
  window.addEventListener('beforeunload', (e) => e.stopImmediatePropagation(), true);

  const invoke = (cmd, args) =>
    window.__TAURI_INTERNALS__
      ? window.__TAURI_INTERNALS__.invoke(cmd, args).catch((e) => console.warn('[ytmd]', cmd, e))
      : Promise.resolve(null);

  const player = () => document.getElementById('movie_player');
  const video = () => document.querySelector('video');
  const barButton = (sel) => document.querySelector(`ytmusic-player-bar ${sel}`);

  function click(sel) {
    const b = barButton(sel);
    if (!b) return false;
    b.click();
    return true;
  }

  window.__ytmd = {
    toggle() {
      if (click('#play-pause-button')) return;
      const v = video();
      if (v) v.paused ? v.play() : v.pause();
    },
    next() {
      if (!click('.next-button')) player()?.nextVideo?.();
    },
    prev() {
      if (!click('.previous-button')) player()?.previousVideo?.();
    },
  };

  // Player bar byline looks like "Artist • Album • 2026" (or "Artist • 1.2M views" for videos).
  function readByline() {
    const byline = document.querySelector('ytmusic-player-bar .byline');
    if (!byline) return { artist: '', album: '' };
    const artist = (byline.textContent || '').split('•')[0].trim();
    const albumLink = [...byline.querySelectorAll('a')].find((a) => a.href.includes('browse/MPRE'));
    return { artist, album: albumLink?.textContent.trim() || '' };
  }

  // YouTube Music plays tracks back to back in one media stream, so the <video> element's
  // time and duration run on across songs. The player API gives them for the current song.
  function songTime(p, v) {
    const t = p?.getCurrentTime?.();
    return Number.isFinite(t) ? t : v?.currentTime || 0;
  }
  // getDuration() is also off right after a back-to-back change (it grows as the song buffers),
  // so prefer the song's listed length.
  function songDuration(p, v) {
    const details = p?.getPlayerResponse?.()?.videoDetails;
    const listed = Number(details?.lengthSeconds);
    if (details?.videoId === p?.getVideoData?.()?.video_id && listed > 0) return listed;
    const d = p?.getDuration?.();
    if (Number.isFinite(d) && d > 0) return d;
    return Number.isFinite(v?.duration) ? v.duration : 0;
  }

  function readState() {
    const p = player();
    const v = video();
    const d = p?.getVideoData?.();
    if (!v || !d?.video_id) return null;
    if (p.classList.contains('ad-showing')) return null; // don't save or count ads
    const { artist, album } = readByline();
    return {
      video_id: d.video_id,
      playlist_id: p.getPlaylistId?.() || null,
      time: songTime(p, v),
      playing: !v.paused && !v.ended,
      title: d.title || '',
      artist: artist || (d.author || '').replace(/ - Topic$/, ''),
      album,
      duration: songDuration(p, v),
    };
  }

  let restoring = false;
  function report() {
    if (restoring) return;
    const s = readState();
    if (!s) return;
    rememberSong(s);
    invoke('report_state', { state: s });
  }

  // Wait for the player to load the saved track, then seek and pause (or play).
  async function restore() {
    const r = await invoke('get_restore');
    if (!r) return;
    restoring = true;
    const deadline = Date.now() + 25000;
    let settledSince = 0;
    let mutedByUs = false;

    const done = () => {
      const v = video();
      if (v && mutedByUs) v.muted = false;
      restoring = false;
      report();
    };

    const tick = () => {
      if (Date.now() > deadline) return done();
      const p = player();
      const v = video();
      const d = p?.getVideoData?.();
      if (!v || d?.video_id !== r.video_id || v.readyState < 1) return setTimeout(tick, 200);

      if (r.autoplay) {
        if (v.paused) p.playVideo ? p.playVideo() : v.play();
        return done();
      }

      if (!mutedByUs && !v.muted) {
        v.muted = true;
        mutedByUs = true;
      }
      // Older saves could hold a position past the song's end (see songTime); start those over.
      const dur = songDuration(p, v);
      const target = dur && r.time > dur - 5 ? 0 : r.time;
      if (target > 1 && Math.abs(songTime(p, v) - target) > 2) {
        p.seekTo ? p.seekTo(target, true) : (v.currentTime = target);
        settledSince = 0;
      }
      if (!v.paused) {
        p.pauseVideo ? p.pauseVideo() : v.pause();
        settledSince = 0;
      }
      // YTM sometimes starts playback late, so require it to stay paused for a bit.
      if (!settledSince) settledSince = Date.now();
      if (Date.now() - settledSince > 2000) return done();
      setTimeout(tick, 200);
    };
    tick();
  }

  // In-app shortcut (default Ctrl+D) to show/hide the stats panel.
  let panelKey = '';
  window.__ytmdSetPanelKey = (k) => { panelKey = k; };
  const accelerator = (e) => {
    if (['Control', 'Shift', 'Alt', 'Meta'].includes(e.key)) return '';
    const mods = [e.ctrlKey && 'Ctrl', e.altKey && 'Alt', e.shiftKey && 'Shift', e.metaKey && 'Super'].filter(Boolean);
    const key = e.code.replace(/^Key([A-Z])$/, '$1').replace(/^Digit(\d)$/, '$1');
    return [...mods, key].join('+');
  };
  document.addEventListener('keydown', (e) => {
    if (panelKey && accelerator(e).toLowerCase() === panelKey.toLowerCase()) {
      e.preventDefault();
      e.stopPropagation();
      invoke('toggle_panel');
    }
  }, true);

  // Button to reopen the stats panel. Sits at the same window spot as the panel's own
  // close button (12px from the right edge, 14px down), so it doesn't jump when toggling.
  let panelOpen = true;
  let panelBtn = null;
  function makePanelButton() {
    const NS = 'http://www.w3.org/2000/svg';
    const btn = document.createElement('button');
    btn.title = 'Show stats';
    Object.assign(btn.style, {
      position: 'fixed', top: '14px', width: '32px', height: '32px', zIndex: '2147483647',
      display: 'none', alignItems: 'center', justifyContent: 'center', padding: '0',
      border: '0', borderRadius: '50%', background: 'transparent', color: '#aaa', cursor: 'pointer',
    });
    btn.onmouseenter = () => { btn.style.background = 'rgba(255,255,255,.1)'; btn.style.color = '#fff'; };
    btn.onmouseleave = () => { btn.style.background = 'transparent'; btn.style.color = '#aaa'; };
    btn.onclick = () => invoke('toggle_panel');
    // lucide:chevron-right, flipped to point left.
    const svg = document.createElementNS(NS, 'svg');
    for (const [k, v] of Object.entries({
      width: '20', height: '20', viewBox: '0 0 24 24', fill: 'none', stroke: 'currentColor',
      'stroke-width': '2', 'stroke-linecap': 'round', 'stroke-linejoin': 'round', transform: 'scale(-1,1)',
    })) svg.setAttribute(k, v);
    const path = document.createElementNS(NS, 'path');
    path.setAttribute('d', 'm9 18 6-6-6-6');
    svg.append(path);
    btn.append(svg);
    document.body.append(btn);
    return btn;
  }
  function placePanelButton() {
    if (!panelBtn) return;
    // position:fixed measures from inside the page scrollbar; offset it so the gap is from the window edge.
    const scrollbar = window.innerWidth - document.documentElement.clientWidth;
    panelBtn.style.right = `${12 - scrollbar}px`;
    panelBtn.style.display = panelOpen ? 'none' : 'flex';
  }
  window.__ytmdSetPanelOpen = (open) => {
    panelOpen = open;
    if (!panelBtn && document.body) panelBtn = makePanelButton();
    placePanelButton();
  };
  window.addEventListener('resize', placePanelButton);

  // The player bar can still show the previous song for a moment after a track change.
  // Its 👍 button names the song it belongs to (titles can differ, e.g. "日本語 - English").
  const barShows = (videoId) => {
    const r = document.querySelector('ytmusic-player-bar ytmusic-like-button-renderer');
    return (r?.data || r?.__data?.data)?.target?.videoId === videoId;
  };

  // ---------- ban list: skip banned songs/artists that YouTube Music picks by itself ----------
  let bans = { songs: new Set(), artists: new Set() };
  window.__ytmdSetBans = (b) => {
    bans = {
      songs: new Set(b.songs.map((s) => s.video_id)),
      artists: new Set(b.artists.map((a) => a.toLowerCase())),
    };
  };

  // "A, B & C" plus "(feat. D)" in the title -> [a, b, c, d]
  function creditedArtists(artist, title) {
    const names = (artist || '').split(/\s*(?:,|&|\bfeat\.?|\bft\.?)\s+/i);
    const feat = /[([](?:feat\.?|ft\.?)\s+([^)\]]+)[)\]]/i.exec(title || '');
    if (feat) names.push(...feat[1].split(/\s*(?:,|&)\s*/));
    return names.map((n) => n.trim().toLowerCase()).filter(Boolean);
  }
  window.__ytmdCreditedArtists = creditedArtists;

  // A click/keypress in the page shortly before a track change means you picked it yourself,
  // except the player bar's next/previous buttons (those still let auto-play choose).
  let lastPick = 0;
  const notePick = (e) => {
    if (!e.isTrusted) return;
    if (e.target?.closest?.('ytmusic-player-bar .next-button, ytmusic-player-bar .previous-button')) return;
    lastPick = Date.now();
  };
  document.addEventListener('pointerdown', notePick, true);
  document.addEventListener('keydown', notePick, true);

  // Checked once per track change (the same song can come round again later).
  let checkingFor = '';
  let checkDone = false;
  function checkBanned() {
    if (restoring) return;
    const p = player();
    const d = p?.getVideoData?.();
    if (!d?.video_id || p.classList.contains('ad-showing')) return;
    if (d.video_id !== checkingFor) {
      checkingFor = d.video_id;
      checkDone = false;
    }
    if (checkDone) return;
    // Only trust the byline once the player bar has caught up with this song.
    const byline = barShows(d.video_id) ? readByline().artist : '';
    const artists = creditedArtists(byline || d.author?.replace(/ - Topic$/, ''), d.title);
    const banned = bans.songs.has(d.video_id) || artists.some((a) => bans.artists.has(a));
    if (!banned) {
      if (byline) checkDone = true; // fully checked once the byline is in
      return;
    }
    checkDone = true;
    if (Date.now() - lastPick < 4000) return; // you chose it: let it play (panel shows a badge)
    window.__ytmd.next();
  }
  for (const ev of ['loadedmetadata', 'play', 'timeupdate']) {
    document.addEventListener(ev, checkBanned, true);
  }

  // ---------- 👍/👎 anywhere in YouTube Music: like = liked songs list, dislike = ban ----------
  // Watches YouTube Music's own like/dislike requests, so the player bar, menus and keyboard
  // shortcuts all count. (👎 skips to the next song before the button even updates, so reading
  // the button afterwards would be too late.)
  const likeStatus = () =>
    document.querySelector('ytmusic-player-bar ytmusic-like-button-renderer')?.getAttribute('like-status') || '';
  const songOf = (s) => ({ video_id: s.video_id, title: s.title, artist: s.artist, album: s.album, duration: s.duration });

  // Songs seen recently, for title/artist when a request only names the video.
  const recentSongs = new Map();
  function rememberSong(s) {
    recentSongs.delete(s.video_id);
    recentSongs.set(s.video_id, songOf(s));
    if (recentSongs.size > 50) recentSongs.delete(recentSongs.keys().next().value);
  }

  async function songInfo(videoId) {
    const cur = readState();
    if (cur?.video_id === videoId) return songOf(cur);
    if (recentSongs.has(videoId)) return recentSongs.get(videoId);
    // Liked from a list or menu: look the song up.
    try {
      const d = (await innertube('player', { videoId })).videoDetails || {};
      return { video_id: videoId, title: d.title || '', artist: (d.author || '').replace(/ - Topic$/, ''), album: '', duration: Number(d.lengthSeconds) || 0 };
    } catch {
      return { video_id: videoId, title: '', artist: '', album: '', duration: 0 };
    }
  }

  // before: the song's 👍/👎 state when the request was sent ('' if it isn't the one in the player bar).
  async function onRate(action, videoId, before) {
    if (action === 'like') {
      if (before === 'DISLIKE') invoke('unban_song', { videoId });
      invoke('like_song', { song: await songInfo(videoId) });
    } else if (action === 'dislike') {
      invoke('unlike_song', { videoId }); // YouTube drops it from your likes too
      invoke('dislike_song', { song: await songInfo(videoId) });
    } else if (action === 'removelike') {
      // Clears either a 👍 or a 👎.
      invoke('unlike_song', { videoId });
      if (before === 'DISLIKE') invoke('unban_song', { videoId });
    }
  }

  // Request bodies can be gzip-compressed.
  async function requestJson(input, init) {
    const body = init?.body ?? (input instanceof Request ? await input.clone().arrayBuffer() : null);
    if (body == null) return null;
    let bytes = new Uint8Array(await new Response(body).arrayBuffer());
    if (bytes[0] === 0x1f && bytes[1] === 0x8b) {
      const unzipped = new Blob([bytes]).stream().pipeThrough(new DecompressionStream('gzip'));
      bytes = new Uint8Array(await new Response(unzipped).arrayBuffer());
    }
    return JSON.parse(new TextDecoder().decode(bytes));
  }

  // What the player bar's buttons showed a moment ago. The page can update the button before it
  // sends the request, and "remove" is the same request for clearing a 👍 or a 👎.
  let barRating = { videoId: '', status: '' };
  setInterval(() => {
    const r = document.querySelector('ytmusic-player-bar ytmusic-like-button-renderer');
    barRating = { videoId: (r?.data || r?.__data?.data)?.target?.videoId || '', status: likeStatus() };
  }, 500);

  const pageFetch = window.fetch;
  window.fetch = function (input, init) {
    try {
      const url = typeof input === 'string' ? input : input?.url || '';
      const m = /\/youtubei\/v1\/like\/(like|dislike|removelike)\b/.exec(url);
      if (m) {
        const { videoId: barId, status } = barRating;
        requestJson(input, init)
          .then((json) => {
            const videoId = json?.target?.videoId;
            if (videoId) onRate(m[1], videoId, videoId === barId ? status : '');
          })
          .catch((e) => console.warn('[ytmd] rate', e));
      }
    } catch (e) {
      console.warn('[ytmd] rate', e);
    }
    return pageFetch.apply(this, arguments);
  };

  // ---------- liked songs sync: reads the "Liked Music" playlist through YouTube Music's own API ----------
  async function sha1(text) {
    const buf = await crypto.subtle.digest('SHA-1', new TextEncoder().encode(text));
    return [...new Uint8Array(buf)].map((b) => b.toString(16).padStart(2, '0')).join('');
  }

  // Same request the page makes itself, signed with your session (SAPISIDHASH).
  async function innertube(endpoint, body, query = '') {
    const cfg = window.ytcfg;
    const headers = {
      'Content-Type': 'application/json',
      'X-Origin': location.origin,
      'X-Goog-AuthUser': String(cfg.get('SESSION_INDEX') ?? 0),
    };
    const sapisid = /(?:^|;\s*)(?:SAPISID|__Secure-3PAPISID)=([^;]+)/.exec(document.cookie)?.[1];
    if (sapisid) {
      const ts = Math.floor(Date.now() / 1000);
      headers.Authorization = `SAPISIDHASH ${ts}_${await sha1(`${ts} ${sapisid} ${location.origin}`)}`;
    }
    const context = structuredClone(cfg.get('INNERTUBE_CONTEXT'));
    // Brand channels: without this you'd get the main Google account's library instead.
    const pageId = cfg.get('DELEGATED_SESSION_ID');
    if (pageId) {
      headers['X-Goog-PageId'] = pageId;
      context.user = { ...context.user, onBehalfOfUser: pageId };
    }
    const r = await fetch(`/youtubei/v1/${endpoint}?prettyPrint=false${query}`, {
      method: 'POST',
      credentials: 'include',
      headers,
      body: JSON.stringify({ context, ...body }),
    });
    if (!r.ok) throw new Error(`YouTube Music answered ${r.status}`);
    return r.json();
  }

  function walk(o, fn) {
    if (!o || typeof o !== 'object') return;
    fn(o);
    for (const v of Object.values(o)) walk(v, fn);
  }
  const runsText = (t) => t?.runs?.map((r) => r.text).join('') ?? t?.simpleText ?? '';
  const toSecs = (hms) => (hms || '').split(':').reduce((a, n) => a * 60 + (Number(n) || 0), 0);

  function playlistItem(it) {
    let videoId = it.playlistItemData?.videoId;
    if (!videoId) walk(it.overlay, (o) => { videoId ||= o.watchEndpoint?.videoId; });
    if (!videoId) return null; // unavailable/removed songs
    const col = (i) => runsText(it.flexColumns?.[i]?.musicResponsiveListItemFlexColumnRenderer?.text);
    return {
      video_id: videoId,
      title: col(0),
      artist: col(1),
      album: col(2),
      duration: toSecs(runsText(it.fixedColumns?.[0]?.musicResponsiveListItemFixedColumnRenderer?.text)),
    };
  }

  // Songs and the next-page token, wherever this page of the response keeps them.
  function playlistPage(json) {
    const songs = [];
    let next = null;
    walk(json, (o) => {
      if (o.musicResponsiveListItemRenderer) {
        const s = playlistItem(o.musicResponsiveListItemRenderer);
        if (s) songs.push(s);
      }
      if (!next && o.continuationCommand?.token) next = { body: { continuation: o.continuationCommand.token }, query: '' };
      if (!next && o.nextContinuationData?.continuation) {
        const t = encodeURIComponent(o.nextContinuationData.continuation);
        next = { body: {}, query: `&ctoken=${t}&continuation=${t}&type=next` };
      }
    });
    return { songs, next };
  }

  async function fetchPlaylist(playlistId, onProgress) {
    const seen = new Set();
    const songs = [];
    let page = playlistPage(await innertube('browse', { browseId: `VL${playlistId}` }));
    for (let pages = 0; pages < 500; pages++) {
      let added = 0;
      for (const s of page.songs) {
        if (!seen.has(s.video_id)) {
          seen.add(s.video_id);
          songs.push(s);
          added++;
        }
      }
      onProgress(songs.length);
      if (!page.next || !added) break;
      page = playlistPage(await innertube('browse', page.next.body, page.next.query));
    }
    return songs;
  }

  let syncing = false;
  window.__ytmdSyncLikes = async () => {
    if (syncing) return;
    syncing = true;
    const status = (s) => invoke('likes_sync_status', { status: s });
    try {
      status({ state: 'running', count: 0 });
      if (!window.ytcfg?.get('LOGGED_IN')) throw new Error('Sign in to YouTube Music first');
      const songs = await fetchPlaylist('LM', (count) => status({ state: 'running', count }));
      await invoke('save_likes', { songs });
      status({ state: 'done', count: songs.length });
    } catch (e) {
      status({ state: 'error', message: String(e?.message || e) });
    } finally {
      syncing = false;
    }
  };

  // Play a song picked in the panel, without reloading the page.
  window.__ytmdPlay = (videoId, playlistId) => {
    lastPick = Date.now(); // your pick: plays even if it's banned
    const endpoint = { watchEndpoint: { videoId, ...(playlistId ? { playlistId } : {}) } };
    const app = document.querySelector('ytmusic-app');
    if (app) {
      app.dispatchEvent(new CustomEvent('yt-navigate', { bubbles: true, composed: true, detail: { endpoint } }));
    } else {
      location.href = `/watch?v=${encodeURIComponent(videoId)}${playlistId ? `&list=${playlistId}` : ''}`;
    }
  };

  // Media events don't bubble, but they can be caught in the capture phase.
  for (const ev of ['play', 'pause', 'loadedmetadata', 'seeked', 'ended']) {
    document.addEventListener(ev, () => setTimeout(report, 500), true);
  }
  setInterval(report, 5000);

  // ---------- equalizer ----------
  // <video> -> headroom -> 10 filters -> limiter -> speakers. Hooked up only once the EQ is first
  // turned on: after that the element's audio can only leave through this graph, so turning it
  // off just routes around the filters.
  const EQ_FREQS = [32, 64, 125, 250, 500, 1000, 2000, 4000, 8000, 16000];
  let eq = { enabled: false, gains: EQ_FREQS.map(() => 0) };
  let audio = null; // { ctx, headroom, filters, limiter }
  const eqSources = new WeakMap(); // <video> -> its MediaElementSource (one per element, ever)
  let eqVideo = null;
  let eqRoute = null; // what eqVideo's source feeds: the filters, or straight to the speakers

  function buildGraph() {
    // Default latency (~10 ms); 'playback' adds ~60 ms, enough to put music videos out of lip-sync.
    const ctx = new AudioContext();
    const headroom = ctx.createGain();
    const filters = EQ_FREQS.map((f, i) => {
      const b = ctx.createBiquadFilter();
      b.type = i === 0 ? 'lowshelf' : i === EQ_FREQS.length - 1 ? 'highshelf' : 'peaking';
      b.frequency.value = f;
      b.Q.value = 1.1;
      return b;
    });
    // Only catches peaks near full scale that the headroom didn't cover.
    const limiter = ctx.createDynamicsCompressor();
    limiter.threshold.value = -1;
    limiter.knee.value = 0;
    limiter.ratio.value = 20;
    limiter.attack.value = 0.003;
    limiter.release.value = 0.25;
    [headroom, ...filters, limiter].reduce((a, b) => (a.connect(b), b));
    limiter.connect(ctx.destination);
    return { ctx, headroom, filters, limiter };
  }

  function applyEq() {
    const v = video();
    if (!v || (!eq.enabled && !audio)) return;
    audio ||= buildGraph();
    const { ctx, headroom, filters } = audio;
    const route = eq.enabled ? headroom : ctx.destination;
    if (v !== eqVideo || route !== eqRoute) {
      let src = eqSources.get(v);
      if (!src) {
        try {
          src = ctx.createMediaElementSource(v);
        } catch (e) {
          return console.warn('[ytmd] eq', e);
        }
        eqSources.set(v, src);
      }
      src.disconnect();
      src.connect(route);
      eqVideo = v;
      eqRoute = route;
    }
    const t = ctx.currentTime;
    filters.forEach((f, i) => f.gain.setTargetAtTime(eq.enabled ? eq.gains[i] || 0 : 0, t, 0.02));
    // Turn everything down by the biggest boost so it has room to go up.
    const boost = eq.enabled ? Math.max(0, ...eq.gains) : 0;
    headroom.gain.setTargetAtTime(10 ** (-boost / 20), t, 0.02);
    if (ctx.state !== 'running') ctx.resume().catch(() => {});
  }

  window.__ytmdSetEq = (e) => {
    eq = e;
    applyEq();
  };
  // Picks up a new <video> element, and wakes the audio context if it was suspended.
  for (const ev of ['play', 'loadedmetadata']) document.addEventListener(ev, applyEq, true);

  // Tauri's IPC bridge isn't ready while this script first runs, so talk to the app after load.
  function start() {
    restore();
    invoke('get_bans').then((b) => { if (b) window.__ytmdSetBans(b); });
    invoke('get_eq').then((e) => { if (e) window.__ytmdSetEq(e); });
    invoke('get_panel_ui').then((r) => {
      if (!r) return;
      panelKey = r.key;
      window.__ytmdSetPanelOpen(r.open);
    });
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', start, { once: true });
  } else {
    start();
  }
})();
