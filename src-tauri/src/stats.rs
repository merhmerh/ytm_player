//! Listening history in SQLite (stats.db in the app data folder).
//!
//! One row per play. `listened` is real playback time (seeks and pauses don't count);
//! `counted` flips on once you've heard half the song or 4 minutes (Last.fm's rule).

use std::{
    path::Path,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::PlayerState;

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
CREATE TABLE IF NOT EXISTS plays (
    id         INTEGER PRIMARY KEY,
    video_id   TEXT    NOT NULL,
    title      TEXT    NOT NULL,
    artist     TEXT    NOT NULL,
    album      TEXT    NOT NULL DEFAULT '',
    duration   REAL    NOT NULL DEFAULT 0,
    started_at INTEGER NOT NULL,          -- unix seconds
    listened   REAL    NOT NULL DEFAULT 0, -- seconds actually played
    counted    INTEGER NOT NULL DEFAULT 0  -- 1 once it passes the listen threshold
);
CREATE INDEX IF NOT EXISTS plays_started_at ON plays(started_at);
CREATE INDEX IF NOT EXISTS plays_video_id ON plays(video_id);
CREATE TABLE IF NOT EXISTS banned_songs (
    video_id TEXT PRIMARY KEY,
    title    TEXT NOT NULL,
    artist   TEXT NOT NULL,
    added_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS banned_artists (
    name     TEXT PRIMARY KEY COLLATE NOCASE,
    added_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS liked_songs (
    video_id TEXT    PRIMARY KEY,
    title    TEXT    NOT NULL,
    artist   TEXT    NOT NULL,
    album    TEXT    NOT NULL DEFAULT '',
    duration REAL    NOT NULL DEFAULT 0,
    ord      INTEGER NOT NULL             -- higher = liked more recently
);
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
";

/// Start of today in local time, as unix seconds.
const TODAY: &str = "CAST(strftime('%s', 'now', 'localtime', 'start of day', 'utc') AS INTEGER)";

/// The play currently being tracked.
struct Current {
    video_id: String,
    play_id: Option<i64>,
    started_at: i64,
    last_media: f64,
    last_wall: Instant,
    listened: f64,
    counted: bool,
}

pub struct Stats {
    conn: Connection,
    cur: Option<Current>,
}

#[derive(Serialize)]
pub struct HistoryItem {
    video_id: String,
    title: String,
    artist: String,
    started_at: i64,
}

#[derive(Serialize, Default)]
pub struct Summary {
    today_plays: i64,
    today_secs: f64,
    total_plays: i64,
    total_secs: f64,
    /// How many times the current song has been counted, including now.
    now_play_count: i64,
    history: Vec<HistoryItem>,
}

#[derive(Serialize, Clone)]
pub struct BannedSong {
    video_id: String,
    title: String,
    artist: String,
}

#[derive(Serialize, Clone, Default)]
pub struct Bans {
    songs: Vec<BannedSong>,
    artists: Vec<String>,
}

/// A song in your YouTube Music "Liked Music" playlist.
#[derive(Serialize, Deserialize, Clone)]
pub struct LikedSong {
    pub video_id: String,
    pub title: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    album: String,
    #[serde(default)]
    duration: f64,
}

#[derive(Serialize, Default)]
pub struct Likes {
    songs: Vec<LikedSong>,
    /// Unix seconds of the last full sync, if any.
    synced_at: Option<i64>,
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

impl Stats {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn, cur: None })
    }

    /// Called with every player report (every ~5s and on play/pause/track change).
    pub fn record(&mut self, s: &PlayerState) -> rusqlite::Result<()> {
        let new_play = match &self.cur {
            None => true,
            // Different song, or the same song started over (repeat / replay).
            Some(c) => c.video_id != s.video_id || (s.time < 3.0 && c.last_media > 30.0),
        };
        if new_play {
            self.cur = Some(Current {
                video_id: s.video_id.clone(),
                play_id: None,
                started_at: unix_now(),
                last_media: s.time,
                last_wall: Instant::now(),
                listened: 0.0,
                counted: false,
            });
            return Ok(());
        }

        let c = self.cur.as_mut().unwrap();
        let media_delta = s.time - c.last_media;
        let wall_delta = c.last_wall.elapsed().as_secs_f64();
        c.last_media = s.time;
        c.last_wall = Instant::now();
        // Only count time that moved at roughly real speed; a jump means a seek.
        if !(media_delta > 0.0 && media_delta <= wall_delta * 1.5 + 2.0) {
            return Ok(());
        }
        c.listened += media_delta;
        let threshold = if s.duration > 0.0 { (s.duration / 2.0).min(240.0) } else { 240.0 };
        c.counted |= c.listened >= threshold;

        // Title/artist are rewritten each time: the player bar can lag a moment behind a track change.
        match c.play_id {
            None => {
                self.conn.execute(
                    "INSERT INTO plays (video_id, title, artist, album, duration, started_at, listened, counted)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![s.video_id, s.title, s.artist, s.album, s.duration, c.started_at, c.listened, c.counted],
                )?;
                c.play_id = Some(self.conn.last_insert_rowid());
                if s.duration > 0.0 {
                    self.fix_old_durations(&s.video_id, s.duration)?;
                }
            }
            Some(id) => {
                self.conn.execute(
                    "UPDATE plays SET title = ?2, artist = ?3, album = ?4, duration = ?5, listened = ?6, counted = ?7
                     WHERE id = ?1",
                    params![id, s.title, s.artist, s.album, s.duration, c.listened, c.counted],
                )?;
            }
        }
        Ok(())
    }

    /// Earlier versions saved song lengths that ran on across tracks (too long), which made
    /// some plays miss the listen mark. When a song plays again, correct its old rows.
    fn fix_old_durations(&self, video_id: &str, duration: f64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE plays SET duration = ?2, counted = (listened >= MIN(?2 / 2.0, 240.0))
             WHERE video_id = ?1 AND duration > ?2 + 5",
            params![video_id, duration],
        )?;
        Ok(())
    }

    pub fn summary(&self, video_id: &str) -> rusqlite::Result<Summary> {
        let (today_plays, today_secs): (i64, f64) = self.conn.query_row(
            &format!(
                "SELECT COALESCE(SUM(counted), 0), COALESCE(SUM(listened), 0) FROM plays WHERE started_at >= {TODAY}"
            ),
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let (total_plays, total_secs): (i64, f64) = self.conn.query_row(
            "SELECT COALESCE(SUM(counted), 0), COALESCE(SUM(listened), 0) FROM plays",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let now_play_count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM plays WHERE video_id = ?1 AND counted = 1",
            [video_id],
            |r| r.get(0),
        )?;
        let mut stmt = self.conn.prepare_cached(
            "SELECT video_id, title, artist, started_at FROM plays WHERE counted = 1 ORDER BY started_at DESC LIMIT 50",
        )?;
        let history = stmt
            .query_map([], |r| {
                Ok(HistoryItem { video_id: r.get(0)?, title: r.get(1)?, artist: r.get(2)?, started_at: r.get(3)? })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Summary { today_plays, today_secs, total_plays, total_secs, now_play_count, history })
    }

    // ---------- ban list ----------

    pub fn bans(&self) -> rusqlite::Result<Bans> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT video_id, title, artist FROM banned_songs ORDER BY added_at DESC")?;
        let songs = stmt
            .query_map([], |r| Ok(BannedSong { video_id: r.get(0)?, title: r.get(1)?, artist: r.get(2)? }))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut stmt = self.conn.prepare_cached("SELECT name FROM banned_artists ORDER BY added_at DESC")?;
        let artists = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(Bans { songs, artists })
    }

    pub fn ban_song(&self, video_id: &str, title: &str, artist: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO banned_songs (video_id, title, artist, added_at) VALUES (?1, ?2, ?3, ?4)",
            params![video_id, title, artist, unix_now()],
        )?;
        Ok(())
    }

    pub fn ban_artist(&self, name: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO banned_artists (name, added_at) VALUES (?1, ?2)",
            params![name.trim(), unix_now()],
        )?;
        Ok(())
    }

    pub fn unban_song(&self, video_id: &str) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM banned_songs WHERE video_id = ?1", [video_id])?;
        Ok(())
    }

    pub fn unban_artist(&self, name: &str) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM banned_artists WHERE name = ?1", [name])?;
        Ok(())
    }

    // ---------- liked songs ----------

    pub fn likes(&self) -> rusqlite::Result<Likes> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT video_id, title, artist, album, duration FROM liked_songs ORDER BY ord DESC")?;
        let songs = stmt
            .query_map([], |r| {
                Ok(LikedSong {
                    video_id: r.get(0)?,
                    title: r.get(1)?,
                    artist: r.get(2)?,
                    album: r.get(3)?,
                    duration: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let synced_at = self
            .conn
            .query_row("SELECT value FROM meta WHERE key = 'likes_synced_at'", [], |r| r.get::<_, String>(0))
            .ok()
            .and_then(|v| v.parse().ok());
        Ok(Likes { songs, synced_at })
    }

    /// Replace the whole table with a fresh copy of the playlist (newest like first).
    pub fn replace_likes(&mut self, songs: &[LikedSong]) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM liked_songs", [])?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR IGNORE INTO liked_songs (video_id, title, artist, album, duration, ord)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            let n = songs.len() as i64;
            for (i, s) in songs.iter().enumerate() {
                stmt.execute(params![s.video_id, s.title, s.artist, s.album, s.duration, n - i as i64])?;
            }
        }
        tx.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('likes_synced_at', ?1)",
            [unix_now().to_string()],
        )?;
        tx.commit()
    }

    /// Add a song you liked in the player. An already-listed song keeps its place.
    pub fn like(&self, s: &LikedSong) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO liked_songs (video_id, title, artist, album, duration, ord)
             VALUES (?1, ?2, ?3, ?4, ?5, (SELECT COALESCE(MAX(ord), 0) + 1 FROM liked_songs))
             ON CONFLICT(video_id) DO UPDATE SET
                 title = excluded.title, artist = excluded.artist, album = excluded.album, duration = excluded.duration",
            params![s.video_id, s.title, s.artist, s.album, s.duration],
        )?;
        Ok(())
    }

    pub fn unlike(&self, video_id: &str) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM liked_songs WHERE video_id = ?1", [video_id])?;
        Ok(())
    }
}
