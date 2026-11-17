
    CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT);
    CREATE TABLE IF NOT EXISTS polls (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        name TEXT UNIQUE NOT NULL,
        emoji TEXT DEFAULT '🎮',
        color TEXT DEFAULT '#5865F2',
        position INTEGER DEFAULT 0,
        active INTEGER DEFAULT 1,
        is_community INTEGER DEFAULT 0,
        message_id TEXT,
        thread_id TEXT,
        closes_at REAL
    );
    CREATE TABLE IF NOT EXISTS options (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        poll_id INTEGER NOT NULL REFERENCES polls(id) ON DELETE CASCADE,
        label TEXT NOT NULL,
        suggestion_id INTEGER
    );
    CREATE TABLE IF NOT EXISTS votes (
        option_id INTEGER NOT NULL REFERENCES options(id) ON DELETE CASCADE,
        user_id INTEGER NOT NULL,
        PRIMARY KEY (option_id, user_id)
    );
    CREATE TABLE IF NOT EXISTS vote_change_counts (
        poll_id INTEGER NOT NULL,
        user_id INTEGER NOT NULL,
        change_count INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (poll_id, user_id)
    );
    CREATE TABLE IF NOT EXISTS suggestions (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        week INTEGER NOT NULL,
        norm_name TEXT NOT NULL,
        name TEXT NOT NULL,
        tipo TEXT NOT NULL,
        link TEXT,
        status TEXT DEFAULT 'pending',   -- pending | picked
        review_status TEXT NOT NULL DEFAULT 'approved',
        picked_by INTEGER,
        picked_at TEXT
    );
    CREATE TABLE IF NOT EXISTS picked_suggestions (
        original_suggestion_id INTEGER PRIMARY KEY,
        week INTEGER NOT NULL,
        norm_name TEXT NOT NULL,
        name TEXT NOT NULL,
        tipo TEXT NOT NULL,
        link TEXT,
        picked_by INTEGER,
        picked_at TEXT
    );
    CREATE INDEX IF NOT EXISTS picked_suggestions_name_idx
        ON picked_suggestions(norm_name, tipo, week);
    CREATE TABLE IF NOT EXISTS poll_message_claims (
        poll_id INTEGER PRIMARY KEY,
        expires_at REAL NOT NULL
    );
    CREATE TABLE IF NOT EXISTS proposals (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        suggestion_id INTEGER NOT NULL,
        user_id INTEGER NOT NULL,
        week INTEGER NOT NULL,
        ts INTEGER NOT NULL,
        raw_name TEXT,
        raw_link TEXT
    );
    CREATE TABLE IF NOT EXISTS suggestion_supports (
        suggestion_id INTEGER NOT NULL REFERENCES suggestions(id) ON DELETE CASCADE,
        user_id INTEGER NOT NULL,
        week INTEGER NOT NULL,
        created_at INTEGER NOT NULL,
        PRIMARY KEY (suggestion_id, user_id)
    );
    CREATE INDEX IF NOT EXISTS suggestion_supports_user_week_idx
        ON suggestion_supports(user_id, week);
    CREATE TABLE IF NOT EXISTS user_stats (
        user_id INTEGER PRIMARY KEY,
        proposals_submitted INTEGER NOT NULL DEFAULT 0,
        supports_given INTEGER NOT NULL DEFAULT 0,
        supports_received INTEGER NOT NULL DEFAULT 0,
        ballot_entries INTEGER NOT NULL DEFAULT 0,
        winning_proposals INTEGER NOT NULL DEFAULT 0
    );
    CREATE TABLE IF NOT EXISTS user_winning_proposals (
        user_id INTEGER NOT NULL,
        week INTEGER NOT NULL,
        suggestion_id INTEGER NOT NULL,
        name TEXT NOT NULL,
        votes INTEGER NOT NULL,
        recorded_at TEXT NOT NULL,
        PRIMARY KEY (user_id, week, suggestion_id)
    );
    CREATE TABLE IF NOT EXISTS weekly_activity (
        user_id INTEGER NOT NULL,
        week INTEGER NOT NULL,
        activity_count INTEGER NOT NULL DEFAULT 0,
        points INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (user_id, week)
    );
    CREATE TABLE IF NOT EXISTS seasons (
        season_key TEXT PRIMARY KEY,
        label TEXT NOT NULL,
        starts_at TEXT NOT NULL,
        ends_at TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS season_activity (
        season_key TEXT NOT NULL REFERENCES seasons(season_key) ON DELETE CASCADE,
        user_id INTEGER NOT NULL,
        week INTEGER NOT NULL,
        activity_count INTEGER NOT NULL DEFAULT 0,
        points INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (season_key, user_id, week)
    );
    CREATE INDEX IF NOT EXISTS season_activity_leaderboard_idx
        ON season_activity(season_key, points DESC, user_id);
    CREATE TABLE IF NOT EXISTS user_streaks (
        user_id INTEGER PRIMARY KEY,
        current_streak INTEGER NOT NULL DEFAULT 0,
        best_streak INTEGER NOT NULL DEFAULT 0,
        last_active_week INTEGER
    );
    CREATE TABLE IF NOT EXISTS weekly_awards (
        week INTEGER PRIMARY KEY,
        user_ids TEXT NOT NULL,
        points INTEGER NOT NULL,
        recorded_at TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS weekly_streak_records (
        user_id INTEGER NOT NULL,
        week INTEGER NOT NULL,
        streak INTEGER NOT NULL,
        PRIMARY KEY (user_id, week)
    );
    CREATE TABLE IF NOT EXISTS community_records (
        record_key TEXT PRIMARY KEY,
        metric INTEGER NOT NULL,
        label TEXT NOT NULL,
        week INTEGER NOT NULL,
        user_id INTEGER
    );
    CREATE TABLE IF NOT EXISTS user_notification_preferences (
        user_id INTEGER PRIMARY KEY,
        dm_enabled INTEGER NOT NULL DEFAULT 0
    );
    CREATE TABLE IF NOT EXISTS user_hidden_badges (
        user_id INTEGER NOT NULL,
        badge_key TEXT NOT NULL,
        unlocked_at TEXT NOT NULL,
        PRIMARY KEY (user_id, badge_key)
    );
    CREATE TABLE IF NOT EXISTS user_ballot_weeks (
        user_id INTEGER NOT NULL,
        week INTEGER NOT NULL,
        PRIMARY KEY (user_id, week)
    );
    CREATE TABLE IF NOT EXISTS history (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        week INTEGER,
        closed_at TEXT,
        data TEXT
    );
    CREATE TABLE IF NOT EXISTS live_events (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        guild_id INTEGER NOT NULL,
        scheduled_event_id INTEGER,
        title TEXT NOT NULL,
        channel_id INTEGER NOT NULL,
        channel_name TEXT NOT NULL,
        started_at TEXT NOT NULL,
        last_seen_at TEXT NOT NULL,
        ended_at TEXT,
        peak_participants INTEGER NOT NULL DEFAULT 0,
        participant_count INTEGER NOT NULL DEFAULT 0,
        end_reason TEXT NOT NULL DEFAULT 'manual'
    );
    CREATE TABLE IF NOT EXISTS live_event_participants (
        event_id INTEGER NOT NULL REFERENCES live_events(id) ON DELETE CASCADE,
        user_id INTEGER NOT NULL,
        joined_at TEXT,
        participation_seconds INTEGER NOT NULL DEFAULT 0,
        speaking_seconds REAL NOT NULL DEFAULT 0,
        PRIMARY KEY (event_id, user_id)
    );
    CREATE INDEX IF NOT EXISTS live_event_participants_speaking_idx
        ON live_event_participants(event_id, speaking_seconds DESC);
    CREATE TABLE IF NOT EXISTS staff_audit (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        occurred_at TEXT NOT NULL,
        actor_id INTEGER,
        action TEXT NOT NULL,
        entity_type TEXT,
        entity_id TEXT,
        reason TEXT,
        details TEXT NOT NULL DEFAULT '{}'
    );
    CREATE INDEX IF NOT EXISTS staff_audit_occurred_idx
        ON staff_audit(occurred_at DESC);
    CREATE TABLE IF NOT EXISTS community_activity (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        action TEXT NOT NULL,
        occurred_at INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS community_activity_time_idx
        ON community_activity(occurred_at, action);
    CREATE TABLE IF NOT EXISTS web_sessions (
        session_id TEXT PRIMARY KEY,
        user_id INTEGER NOT NULL,
        display_name TEXT NOT NULL,
        created_at TEXT NOT NULL,
        expires_at TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS site_report_notifications (
        report_id TEXT PRIMARY KEY,
        message_id TEXT NOT NULL,
        sent_at TEXT NOT NULL
    );
    